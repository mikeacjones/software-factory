//! `factory`: run one unit by hand, and look at what the factory knows.

use std::path::PathBuf;
use std::process::ExitCode;

use factory_beads::readiness;
use factory_config::ConfigDir;
use factory_driver::{Driver, Options, Status, bead_source, load_flow, usage_report};

const USAGE: &str = "\
usage:
  factory run <project> <bead> [--no-slack]   walk one bead through its project's flow
  factory run --unit <unit> [--no-slack]      continue a stopped or parked unit
  factory bead <project> <bead>               show a bead and whether it may be launched
  factory flow check <project>                check the flow a project names

options:
  --config <dir>   the config directory (default: $FACTORY_CONFIG_DIR, or ~/.config/software-factory)

exit codes: 0 the flow ended, 2 the unit is parked, 1 anything else";

struct Args {
    config: ConfigDir,
    slack: bool,
    unit: Option<String>,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        config: ConfigDir::default_location(),
        slack: true,
        unit: None,
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
            let options = Options { slack: args.slack };
            let mut driver = match (&args.unit, rest) {
                (Some(unit), []) => Driver::resume(args.config, unit, &options),
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
                    match &driver.unit().pull_request {
                        Some(url) => println!("\nended: pull request {url}"),
                        None => println!("\nended: branch {} pushed", driver.unit().branch),
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
