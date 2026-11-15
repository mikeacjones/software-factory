//! `factory`: run one unit by hand, and look at what the factory knows.

use std::path::PathBuf;
use std::process::ExitCode;

use factory_beads::readiness;
use factory_config::ConfigDir;
use factory_driver::goal::{Supervisor, check_definitions};
use factory_driver::{
    AccountPick, Driver, Options, Status, bead_source, load_flow, pick_account, sync_issue_fields,
    usage_report,
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
  factory service install                     keep the service running under launchd, in this
                                              user's desktop session: make a release of this
                                              build and this config, and have launchd run it
                                              at login and whenever it exits
  factory service deploy [--now]              make a release of this build and this config
                                              current, and restart the service onto it
  factory service restart [--now]             restart the service at a safe point: running
                                              steps finish and no other starts. `--now` stops
                                              it where it stands, and the steps run again
  factory service status                      whether launchd is running it, on which release
  factory service uninstall                   stop it and take it out of launchd
  factory issues sync <project>               set the type and priority of every open bead's
                                              GitHub issue to the bead's, where they differ
  factory accounts                            each Claude account's usage windows, and which
                                              one a run would use now
  factory accounts refresh                    refresh the usage login of each account marked for
                                              it now, write it to the Keychain and check it works
  factory accounts pin <account>              accept whichever account the stored login of this
                                              account now belongs to, after a deliberate change
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
    /// Do it at once, without waiting for a safe point.
    now: bool,
    unit: Option<String>,
    from: Option<String>,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        config: ConfigDir::default_location(),
        slack: true,
        now: false,
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
            "--now" => parsed.now = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            value => parsed.positional.push(value.to_string()),
        }
    }
    Ok(parsed)
}

/// Say how launchd sees the service. Returns whether it is running.
fn service_status(factory: &factory_config::Factory, job: &factory_driver::service::Job) -> bool {
    use factory_driver::service::{children, current, started_by_hand};
    let state = job.state();
    let release = std::fs::canonicalize(current(&factory.state_dir))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "none".to_string());
    match state.pid {
        Some(pid) => println!(
            "launchd job `{}`: {}, process {pid}, {} step(s) in flight",
            job.label,
            state.state,
            children(pid).len()
        ),
        None if state.state.is_empty() => {
            println!("launchd job `{}`: not loaded", job.label);
        }
        None => println!(
            "launchd job `{}`: {}, last exit {}",
            job.label,
            state.state,
            state.last_exit.as_deref().unwrap_or("unknown")
        ),
    }
    println!("current release: {release}");
    if factory_driver::draining(&factory.state_dir) {
        println!("it has been asked to stop at a safe point, and is letting running steps finish");
    }
    for pid in started_by_hand(state.pid) {
        println!(
            "LOOK AT THIS: a factory service launchd did not start is running (process {pid})"
        );
    }
    state.pid.is_some()
}

/// Restart the service onto the current release: at a safe point, or at
/// once. Waits until launchd runs it under a new process.
fn service_restart(
    factory: &factory_config::Factory,
    job: &factory_driver::service::Job,
    now: bool,
) -> Result<u8, String> {
    use factory_driver::service::children;
    let before = job.state();
    let Some(old) = before.pid else {
        if before.state.is_empty() {
            return Err(format!(
                "launchd has no job `{}`: run `factory service install` first",
                job.label
            ));
        }
        job.kickstart(false).map_err(|err| err.to_string())?;
        println!("it was not running, and has been started");
        return Ok(0);
    };
    if now {
        job.kickstart(true).map_err(|err| err.to_string())?;
    } else {
        let flag = factory_driver::drain_flag(&factory.state_dir);
        std::fs::write(&flag, "").map_err(|err| format!("{}: {err}", flag.display()))?;
        println!(
            "asked process {old} to stop at a safe point: {} step(s) are in flight and will finish first",
            children(old).len()
        );
    }
    let started = std::time::Instant::now();
    let mut told = 0;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match job.state().pid {
            Some(pid) if pid != old => {
                println!("restarted: process {pid}");
                service_status(factory, job);
                return Ok(0);
            }
            _ => {}
        }
        let minutes = started.elapsed().as_secs() / 60;
        if minutes > told {
            told = minutes;
            println!(
                "still waiting after {minutes} min: {} step(s) in flight",
                children(old).len()
            );
        }
        if started.elapsed() >= std::time::Duration::from_secs(4 * 3600) {
            return Err(
                "it had not restarted after four hours. The request stands; `factory service restart --now` stops it where it is"
                    .to_string(),
            );
        }
    }
}

/// Serve with everything on the engine: the tiers, the pipelines and the
/// accounts are workflows on the Temporal server, and what is left to run
/// here is the engine's worker. This process becomes it, so launchd keeps
/// the worker itself alive, and restarting the service restarts the worker
/// on the current release.
fn serve_on_engine(
    config: &ConfigDir,
    factory: &factory_config::Factory,
    temporal: &factory_config::Temporal,
) -> Result<u8, String> {
    use std::os::unix::process::CommandExt;
    let engine = temporal
        .engine_bin
        .clone()
        .unwrap_or_else(|| factory.state_dir.join("bin/factory-engine"));
    // The engine this release was built with, if the release has one.
    if let Some(release) = config.0.parent() {
        match factory_driver::service::install_engine(release, &engine) {
            Ok(true) => factory_core::note!(
                "serve: installed this release's engine at {}",
                engine.display()
            ),
            Ok(false) => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    factory_core::note!(
        "serve: started from {}; everything runs on the engine, and this process becomes its worker ({})",
        config.0.display(),
        engine.display()
    );
    let err = std::process::Command::new(&engine)
        .args(["worker", "--config"])
        .arg(&config.0)
        .exec();
    Err(format!("{} could not be started: {err}", engine.display()))
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
                within: None,
                continued: None,
                ruling: None,
                pausable: false,
                engine: false,
                account: None,
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
            // The release this process started from, whatever the link to
            // the current one is made to name while it runs.
            let config = ConfigDir(std::fs::canonicalize(&args.config.0).unwrap_or(args.config.0));
            let factory = config.factory().map_err(|err| fail(&err))?;
            if let Some(temporal) = factory.temporal.as_ref().filter(|temporal| temporal.tiers) {
                return serve_on_engine(&config, &factory, temporal);
            }
            // What a service that died left running would work beside what
            // this one starts in its place.
            let live_runs = factory_driver::service::live_runs(&factory.state_dir);
            for left in factory_supervise::reap_leftovers(&live_runs) {
                factory_core::note!("serve: killed a run left by an earlier service: {left}");
            }
            factory_supervise::track_long_runs(&live_runs);
            // A request to stop at a safe point was for the service before.
            let _ = std::fs::remove_file(factory_driver::drain_flag(&factory.state_dir));
            let (mut supervisor, listener) =
                Supervisor::from_config(config.clone()).map_err(|err| fail(&err))?;
            factory_core::note!(
                "serve: started from {}; listening for goals on: {}",
                config.0.display(),
                supervisor.projects_listening().join(", ")
            );
            supervisor.resume().map_err(|err| fail(&err))?;
            supervisor.take_parked_incoming();
            supervisor.serve(&listener);
            listener.stop();
            factory_core::note!("serve: stopped at a safe point, as asked");
            Ok(0)
        }
        ["service", "install"] => {
            use factory_driver::service::{Job, plist, release_this_build, started_by_hand};
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            check_definitions(&args.config).map_err(|err| fail(&err))?;
            let job = Job::of(&factory).map_err(|err| fail(&err))?;
            let by_hand = started_by_hand(job.state().pid);
            if !by_hand.is_empty() {
                return Err(format!(
                    "a factory service that launchd did not start is running (process {by_hand:?}). Two would work on the same goals: stop that one first"
                ));
            }
            let release = release_this_build(&factory, &args.config.0).map_err(|err| fail(&err))?;
            println!("release: {}", release.display());
            let shell = factory
                .service
                .login_shell
                .then(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string()));
            let text = plist(
                &job.label,
                &factory.state_dir,
                &factory.service.path,
                shell.as_deref(),
            );
            // Loading a job again stops the one that is running. If it is,
            // it is first asked to stop at a safe point, and the new
            // description is loaded the moment it has: launchd starts it
            // again at once under the old description, and that start is
            // replaced before it has begun anything.
            if let Some(old) = job.state().pid {
                let flag = factory_driver::drain_flag(&factory.state_dir);
                std::fs::write(&flag, "").map_err(|err| format!("{}: {err}", flag.display()))?;
                println!(
                    "the service is running (process {old}): waiting for it to stop at a safe point, {} step(s) in flight",
                    factory_driver::service::children(old).len()
                );
                let started = std::time::Instant::now();
                while job.state().pid == Some(old) {
                    if started.elapsed() >= std::time::Duration::from_secs(4 * 3600) {
                        return Err(
                            "it had not stopped after four hours; nothing was changed".to_string()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
            job.load(&text).map_err(|err| fail(&err))?;
            println!(
                "launchd job `{}` loaded from {}",
                job.label,
                job.plist.display()
            );
            service_status(&factory, &job);
            Ok(0)
        }
        ["service", "deploy"] => {
            use factory_driver::service::{Job, release_this_build};
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            // A config that cannot run must not become the current one.
            check_definitions(&args.config).map_err(|err| fail(&err))?;
            let job = Job::of(&factory).map_err(|err| fail(&err))?;
            let release = release_this_build(&factory, &args.config.0).map_err(|err| fail(&err))?;
            println!("release: {}", release.display());
            service_restart(&factory, &job, args.now)
        }
        ["service", "restart"] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let job = factory_driver::service::Job::of(&factory).map_err(|err| fail(&err))?;
            service_restart(&factory, &job, args.now)
        }
        ["service", "status"] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let job = factory_driver::service::Job::of(&factory).map_err(|err| fail(&err))?;
            Ok(if service_status(&factory, &job) { 0 } else { 1 })
        }
        ["service", "uninstall"] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let job = factory_driver::service::Job::of(&factory).map_err(|err| fail(&err))?;
            job.unload().map_err(|err| fail(&err))?;
            println!("launchd job `{}` stopped and removed", job.label);
            Ok(0)
        }
        ["accounts", "refresh"] => {
            use factory_claude::accounts::{
                Known, Refresh, Refreshed, Renew, TOKEN_URL, USAGE_API, Vault, read_usage,
                refresh_login,
            };
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let now = factory_core::now_unix();
            let tracker = factory_claude::accounts::Tracker::new(&factory.state_dir);
            let states = tracker.load();
            let mut marked = 0;
            for account in factory.accounts.iter().filter(|a| a.usage_refresh) {
                let Some(service) = &account.usage_keychain else {
                    continue;
                };
                marked += 1;
                let refresh = Refresh {
                    token_url: TOKEN_URL,
                    safe_dir: &factory.state_dir,
                    lock_dir: account.usage_lock_dir.as_deref(),
                    lock_tries: factory_claude::accounts::LOCK_TRIES,
                };
                // A login Claude Code also uses is refreshed only when it
                // is due; one nothing else touches, whenever asked.
                let renew = match account.usage_lock_dir {
                    Some(_) => Renew::IfDue { refused: None },
                    None => Renew::Always,
                };
                let vault = Vault::keychain(service);
                let done = refresh_login(&account.id, &vault, refresh, renew, now)
                    .map_err(|err| fail(&err))?;
                // The proof that the login works and is this account's: ask with it.
                let known = states.get(&account.id).cloned().unwrap_or_default();
                let whose = Known {
                    identity: known.identity.as_deref(),
                    login: known.identity_login,
                };
                let proof = match read_usage(&account.id, &vault, USAGE_API, None, whose, now) {
                    Ok(_) => "the usage endpoint accepts it".to_string(),
                    // The endpoint turns callers away for a while at times:
                    // that says nothing about the login.
                    Err(factory_claude::accounts::AccountError::Unanswered { reason, .. }) => {
                        format!(
                            "it is this account's login, but its usage could not be read just now ({reason})"
                        )
                    }
                    Err(err) => return Err(fail(&err)),
                };
                match done {
                    Refreshed::Done => println!(
                        "{}: login refreshed and written to the Keychain item `{service}`; {proof}",
                        account.id
                    ),
                    Refreshed::NotNeeded { minutes_left } => println!(
                        "{}: the login in the Keychain item `{service}` is good for another {minutes_left} minutes and was left alone; {proof}",
                        account.id
                    ),
                }
            }
            if marked == 0 {
                println!("no account is marked `usage_refresh = true`");
            }
            Ok(0)
        }
        ["accounts", "pin", id] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            if !factory.accounts.iter().any(|account| &account.id == id) {
                return Err(format!("the factory has no account `{id}`"));
            }
            factory_claude::accounts::Tracker::new(&factory.state_dir)
                .update(id, |state| {
                    state.identity = None;
                    state.identity_login = 0;
                    state.concern = None;
                })
                .map_err(|err| fail(&err))?;
            println!(
                "{id}: whoever its stored login belongs to at the next check is taken to be this account"
            );
            Ok(0)
        }
        ["issues", "sync", project] => {
            let factory = args.config.factory().map_err(|err| fail(&err))?;
            let project = args.config.project(project).map_err(|err| fail(&err))?;
            let source = bead_source(&project);
            let (mut seen, mut changed, mut failed) = (0, 0, 0);
            for summary in source.list_open().map_err(|err| fail(&err))? {
                let bead = source.read(&summary.id).map_err(|err| fail(&err))?;
                if bead.external_ref.trim().is_empty() {
                    continue;
                }
                seen += 1;
                match sync_issue_fields(&factory, &project, &bead) {
                    Ok(set) if set.is_empty() => {}
                    Ok(set) => {
                        changed += 1;
                        println!("{}  {}: set {}", bead.id, bead.external_ref, set.join(", "));
                    }
                    Err(err) => {
                        failed += 1;
                        println!("{}  {}: {err}", bead.id, bead.external_ref);
                    }
                }
            }
            println!(
                "{seen} open beads have an issue: {changed} brought into step, {failed} failed, the rest already agreed"
            );
            Ok(if failed == 0 { 0 } else { 1 })
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
                println!(
                    "  login:         {}",
                    match (&account.usage_keychain, &state.identity) {
                        (None, _) => "none to read usage with".to_string(),
                        (Some(_), Some(_)) => "confirmed to be this account's".to_string(),
                        (Some(_), None) => "not yet checked".to_string(),
                    }
                );
                if let Some(concern) = &state.concern {
                    println!("  LOOK AT THIS:  {concern}");
                }
            }
            let ids: Vec<String> = factory.accounts.iter().map(|a| a.id.clone()).collect();
            for (first, second) in factory_claude::accounts::same_subscription(&ids, &states, now) {
                println!(
                    "LOOK AT THIS: `{first}` and `{second}` look like one subscription listed twice: their logins or their weekly windows are the same. Balancing between them spends one subscription while counting two."
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
                    "{}  {:?}  {} wakes, {} pipelines, est. ${:.2} for its director and foremen\n    {}",
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
