//! `factory`: run one unit by hand, and look at what the factory knows.

use std::path::PathBuf;
use std::process::ExitCode;

use factory_beads::readiness;
use factory_config::ConfigDir;
use factory_driver::goal::{Supervisor, check_definitions};
use factory_driver::{
    AccountPick, Driver, Options, Status, bead_source, load_flow, pick_account, usage_report,
};

const USAGE: &str = "\
usage:
  factory run <project> <bead> [--no-slack]   walk one bead through its project's flow
  factory run --unit <unit> [--no-slack]      continue a stopped or parked unit
  factory run --unit <unit> --from <step>     owner's override: put the unit at that step
                                              of the project's flow as it is now
  factory serve                               the always-on service: listen to Slack over a
                                              socket, open goals from the owner's messages,
                                              and run them on events
  factory accounts                            each Claude account's usage windows, and which
                                              one a run would use now
  factory slack check                         connect the Slack socket once and say whether it worked
  factory goal check                          check every project's goal definitions
  factory goal list                           the goals the factory knows and where each stands
  factory bead <project> <bead>               show a bead and whether it may be launched
  factory flow check <project>                check the flow a project names

options:
  --config <dir>   the config directory (default: $FACTORY_CONFIG_DIR, or ~/.config/software-factory)

exit codes: 0 the flow ended, 2 the unit is parked, 1 anything else";

struct Args {
    config: ConfigDir,
    slack: bool,
    unit: Option<String>,
    from: Option<String>,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        config: ConfigDir::default_location(),
        slack: true,
        unit: None,
        from: None,
        positional: Vec::new(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let dir = args.next().ok_or("--config needs a directory")?;
                parsed.config = ConfigDir(PathBuf::from(dir));
            }
            "--unit" => parsed.unit = Some(args.next().ok_or("--unit needs a unit ID")?.clone()),
            "--from" => parsed.from = Some(args.next().ok_or("--from needs a step")?.clone()),
            "--no-slack" => parsed.slack = false,
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            value => parsed.positional.push(value.to_string()),
        }
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("factory: {message}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<u8, String> {
    let args = parse(args)?;
    let words: Vec<&str> = args.positional.iter().map(String::as_str).collect();
    let fail = |err: &dyn std::fmt::Display| err.to_string();
    match words.as_slice() {
        ["run", rest @ ..] => {
            let options = Options {
                slack: args.slack,
                flow: None,
            };
            let mut driver = match (&args.unit, rest) {
                (Some(unit), []) => {
                    Driver::resume(args.config, unit, &options, args.from.as_deref())
                }
                (None, [project, bead]) => Driver::start(args.config, project, bead, &options),
                _ => {
                    return Err(format!(
                        "run needs a project and a bead, or --unit\n{USAGE}"
                    ));
                }
            }
            .map_err(|err| fail(&err))?;
            println!("unit:      {}", driver.unit().id);
            println!("worktree:  {}", driver.unit().worktree.display());
            println!("event log: {}", driver.event_log().display());
            let status = driver.run().map_err(|err| fail(&err))?;
            println!("\n{}", usage_report(driver.unit(), driver.flow()));
            match status {
                Status::Ended => {
                    match (&driver.unit().merged, &driver.unit().pull_request) {
                        (Some(commit), Some(url)) => {
                            println!("\nended: {url} merged as {commit}; the bead is closed")
                        }
                        (None, Some(url)) => println!("\nended: pull request {url}"),
                        _ => println!("\nended: branch {} pushed", driver.unit().branch),
                    }
                    Ok(0)
                }
                Status::Parked(reason) => {
                    println!(
                        "\nparked at step `{}`: {reason}",
                        driver.unit().position.step
                    );
                    println!("continue with: factory run --unit {}", driver.unit().id);
                    Ok(2)
                }
                Status::Running => Err("the run stopped without ending or parking".to_string()),
            }
        }
        ["serve"] => {
            let (mut supervisor, listener) =
                Supervisor::from_config(args.config).map_err(|err| fail(&err))?;
            println!(
                "listening for goals on: {}",
                supervisor.projects_listening().join(", ")
            );
            supervisor.resume().map_err(|err| fail(&err))?;
            supervisor.serve(&listener);
            Ok(0)
        }
        ["accounts"] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let now = factory_core::now_unix();
            let tracker = factory_claude::accounts::Tracker::new(&factory.state_dir);
            // Choosing refreshes an account the factory has not heard from lately.
            let choices: Vec<String> = ["claude-opus-5-5", "claude-fable-5-2"]
                .iter()
                .map(|model| {
                    let pick = match pick_account(&factory, model) {
                        Ok(AccountPick::Use { id, .. }) => format!("account `{id}`"),
                        Ok(AccountPick::Wait { until }) => {
                            format!("wait {} minutes", until.saturating_sub(now).div_ceil(60))
                        }
                        Err(err) => format!("no account ({err})"),
                    };
                    format!("a run on {model} now would use: {pick}")
                })
                .collect();
            let states = tracker.load();
            for account in &factory.accounts {
                let state = states.get(&account.id).cloned().unwrap_or_default();
                let window = |window: &Option<factory_claude::Window>| match window {
                    Some(w) if w.resets_at > now => format!(
                        "{:>3.0}%, resets in {}h {:02}m",
                        w.utilization * 100.0,
                        (w.resets_at - now) / 3600,
                        (w.resets_at - now) % 3600 / 60
                    ),
                    Some(_) => "not open".to_string(),
                    None => "unknown".to_string(),
                };
                println!("{}", account.id);
                println!("  session (5h):  {}", window(&state.five_hour));
                println!("  week:          {}", window(&state.seven_day));
                for (scope, until) in state.blocked.iter().filter(|(_, until)| **until > now) {
                    println!(
                        "  at a limit for {}: {} more minutes",
                        if scope == "*" { "every model" } else { scope },
                        (until - now).div_ceil(60)
                    );
                }
                println!(
                    "  learned from:  {}",
                    if state.updated_at == 0 {
                        "nothing yet".to_string()
                    } else {
                        format!(
                            "{}, {} minutes ago",
                            state.source,
                            (now - state.updated_at) / 60
                        )
                    }
                );
            }
            for line in choices {
                println!("{line}");
            }
            Ok(0)
        }
        ["slack", "check"] => {
            let (supervisor, listener) =
                Supervisor::from_config(args.config).map_err(|err| fail(&err))?;
            for _ in 0..40 {
                if listener.connected() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            listener.stop();
            if !listener.connected() {
                return Err("the Slack socket did not connect within ten seconds".to_string());
            }
            println!(
                "the Slack socket is connected; goals would be heard in the channels of: {}",
                supervisor.projects_listening().join(", ")
            );
            Ok(0)
        }
        ["goal", "check"] => {
            let lines = check_definitions(&args.config).map_err(|err| fail(&err))?;
            if lines.is_empty() {
                println!("no project has a goal definition");
            }
            for line in lines {
                println!("{line}");
            }
            Ok(0)
        }
        ["goal", "list"] => {
            let supervisor =
                Supervisor::new(args.config, None, Vec::new(), false).map_err(|err| fail(&err))?;
            for goal in supervisor.goals() {
                println!(
                    "{}  {:?}  {} wakes, {} pipelines, est. ${:.2} for the director\n    {}",
                    goal.id,
                    goal.phase,
                    goal.wakes,
                    goal.units.len(),
                    goal.usage.cost_usd,
                    goal.charter
                        .as_ref()
                        .map(|charter| charter.objective.replace('\n', " "))
                        .unwrap_or_else(|| "(still being agreed)".to_string())
                );
            }
            Ok(0)
        }
        ["bead", project, bead] => {
            let project = args.config.project(project).map_err(|err| fail(&err))?;
            let bead = bead_source(&project).read(bead).map_err(|err| fail(&err))?;
            println!("{}  {}", bead.id, bead.title);
            println!(
                "status {} · type {} · priority {} · assignee {}",
                bead.status,
                bead.issue_type,
                bead.priority,
                if bead.assignee.is_empty() {
                    "-"
                } else {
                    &bead.assignee
                }
            );
            if !bead.acceptance_criteria.trim().is_empty() {
                println!(
                    "\nacceptance criteria:\n{}",
                    bead.acceptance_criteria.trim()
                );
            }
            let reasons = readiness(&bead, &project.bead_actor);
            if reasons.is_empty() {
                println!("\nlaunchable");
                Ok(0)
            } else {
                println!("\nnot launchable:");
                for reason in reasons {
                    println!("  - {reason}");
                }
                Ok(2)
            }
        }
        ["flow", "check", project] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let project = args.config.project(project).map_err(|err| fail(&err))?;
            let (flow, _) =
                load_flow(&args.config, &factory, &project).map_err(|err| fail(&err))?;
            println!(
                "flow `{}` is sound: {} steps, at most {} step runs for one unit",
                flow.id,
                flow.steps.len(),
                flow.longest_run()
            );
            Ok(0)
        }
        _ => Err(format!("unknown command\n{USAGE}")),
    }
}
