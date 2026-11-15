//! The flow engine's own command: its worker, and the calls that start a
//! pipeline and talk to one.
//!
//!   factory-engine worker                     run a worker: the engine and the steps
//!   factory-engine start <project> <bead>     start a pipeline for a bead, and say
//!                                             so if the bead is refused
//!   factory-engine standing <pipeline>        where a pipeline stands
//!   factory-engine go-on <pipeline> <step> <note>
//!                                             continue a parked pipeline from a step
//!   factory-engine give-up <pipeline>         end a parked pipeline
//!   factory-engine accounts                   what every account has left, and who waits
//!   factory-engine accounts recheck           look at every account again now, as after
//!                                             a usage reset; those waiting are released
//!
//!   --config <dir>   the factory's config directory, whose `[temporal]`
//!                    table says where the server is

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use factory_config::{ConfigDir, Temporal};
use factory_engine::accounts::{ACCOUNTS_ID, Accounts};
use factory_engine::engine::{Adopt, GoOn, Pipeline, PipelineInput};
use factory_engine::factory::FactoryRunner;
use factory_engine::worker::{WorkerSettings, connect, ensure_accounts, pipeline_options, run};
use temporalio_client::errors::WorkflowStartError;
use temporalio_client::{
    WorkflowGetResultOptions, WorkflowIdReusePolicy, WorkflowQueryOptions, WorkflowSignalOptions,
};

/// What this build of the engine is called: its release, when it runs
/// from one, so that a pipeline's history says which build ran each step.
/// How long `start` watches a new pipeline for a unit or a refusal, in
/// seconds.
const START_LOOK_SECS: u64 = 90;

fn build_id() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .and_then(|path| {
            let mut parts = path.components().rev().skip(1);
            let release = parts.next()?.as_os_str().to_string_lossy().into_owned();
            (parts.next()?.as_os_str() == "releases").then_some(release)
        })
        .unwrap_or_else(|| "unreleased".to_string())
}

/// A pipeline for something that runs goals to start, as the file it
/// hands over says it.
#[derive(serde::Deserialize)]
struct RunSpec {
    project: String,
    /// The pipeline's ID, when whoever starts it has to be able to find
    /// it again by a name of its own. A pipeline of that name that exists
    /// already, running or ended, is followed and nothing is started.
    #[serde(default)]
    id: Option<String>,
    /// The bead, or what a sub-goal being shipped is called.
    bead: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    flow_name: Option<String>,
    #[serde(default)]
    within: Option<factory_driver::Within>,
    /// A unit to take up where it stands, instead of making one.
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    adopt: Option<Adopt>,
}

/// Follow a pipeline to its end for whoever started it, a line at a time
/// on standard output: `unit <id>` as soon as it has one, then `end` with
/// how it ended, `refused` with why it never began, or `failed` with what
/// went wrong.
async fn follow(
    handle: &temporalio_client::WorkflowHandle<temporalio_client::Client, Pipeline>,
) -> Result<(), String> {
    use std::io::Write;
    let say = |line: String| {
        println!("{line}");
        let _ = std::io::stdout().flush();
    };
    let mut result = Box::pin(handle.get_result(WorkflowGetResultOptions::default()));
    let mut named = false;
    let end = loop {
        tokio::select! {
            end = &mut result => break end,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)), if !named => {
                if let Ok(standing) = handle
                    .query(Pipeline::standing, (), WorkflowQueryOptions::default())
                    .await
                    && !standing.unit.is_empty()
                {
                    say(format!("unit {}", standing.unit));
                    named = true;
                }
            }
        }
    };
    match end {
        Ok(end) => {
            if !named && !end.unit.is_empty() {
                say(format!("unit {}", end.unit));
            }
            say(format!(
                "end {}",
                serde_json::to_string(&end).map_err(|err| err.to_string())?
            ));
        }
        Err(err) => {
            // A pipeline that never began says why to whoever asks.
            let refused = handle
                .query(Pipeline::standing, (), WorkflowQueryOptions::default())
                .await
                .ok()
                .and_then(|standing| standing.refused);
            match refused {
                Some(reason) => say(format!("refused {}", reason.replace('\n', " "))),
                None => say(format!("failed {}", err.to_string().replace('\n', " "))),
            }
        }
    }
    Ok(())
}

async fn command(args: &[String]) -> Result<(), String> {
    let mut config = ConfigDir::default_location();
    let mut wait = false;
    let mut words: Vec<&str> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--config" => {
                config = ConfigDir(PathBuf::from(
                    rest.next().ok_or("--config needs a directory")?,
                ));
            }
            "--wait" => wait = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            word => words.push(word),
        }
    }
    // The release this process started from, whatever is made current later.
    let config = ConfigDir(std::fs::canonicalize(&config.0).unwrap_or(config.0));
    let factory = config.factory().map_err(|err| err.to_string())?;
    let temporal: Temporal = factory
        .temporal
        .clone()
        .ok_or("factory.toml has no [temporal] table")?;
    match words.as_slice() {
        ["worker"] => {
            let settings = WorkerSettings {
                address: temporal.address.clone(),
                namespace: temporal.namespace.clone(),
                task_queue: temporal.task_queue.clone(),
                deployment: temporal.deployment.clone(),
                build_id: build_id(),
                versioned: false,
            };
            eprintln!(
                "factory-engine: worker of build `{}` on {} / {} / {}",
                settings.build_id, settings.address, settings.namespace, settings.task_queue
            );
            let runner = Arc::new(FactoryRunner {
                config,
                slack: factory.slack.is_some(),
            });
            let client = connect(&temporal.address, &temporal.namespace).await?;
            ensure_accounts(&client, &temporal.task_queue, temporal.accounts_check_secs).await?;
            run(settings, runner).await
        }
        ["start", project, bead] => {
            // Refuse a project the factory does not know before anything
            // is started for it.
            config.project(project).map_err(|err| err.to_string())?;
            let client = connect(&temporal.address, &temporal.namespace).await?;
            ensure_accounts(&client, &temporal.task_queue, temporal.accounts_check_secs).await?;
            let id = format!("pipeline-{project}-{bead}-{}", factory_core_now());
            let input = PipelineInput {
                project: project.to_string(),
                bead: bead.to_string(),
                beat_secs: temporal.beat_secs,
                accounts: ACCOUNTS_ID.to_string(),
                ..PipelineInput::default()
            };
            // What the bead is called, for whoever reads the pipeline on
            // the server. A bead that cannot be read is refused by the
            // pipeline itself, with the reason.
            let title = config
                .project(project)
                .ok()
                .and_then(|project| factory_driver::bead_source(&project).read(bead).ok())
                .map(|bead| bead.title)
                .unwrap_or_default();
            let options = pipeline_options(&temporal.task_queue, &id, &input, &title);
            let handle = client
                .start_workflow(Pipeline::run, input, options)
                .await
                .map_err(|err| format!("the pipeline could not be started: {err}"))?;
            println!("{id}");
            if !wait {
                // A bead that may not be worked on is refused in the
                // pipeline's first moments. Whoever started it is told
                // here, not left with a pipeline that ended unseen.
                let began = std::time::Instant::now();
                while began.elapsed() < std::time::Duration::from_secs(START_LOOK_SECS) {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let Ok(standing) = handle
                        .query(Pipeline::standing, (), WorkflowQueryOptions::default())
                        .await
                    else {
                        continue;
                    };
                    if let Some(reason) = standing.refused {
                        return Err(format!("the pipeline ended before its first step: {reason}"));
                    }
                    if !standing.unit.is_empty() {
                        println!("unit: {}", standing.unit);
                        break;
                    }
                }
            }
            if wait {
                let end = handle
                    .get_result(WorkflowGetResultOptions::default())
                    .await
                    .map_err(|err| format!("the pipeline failed: {err}"))?;
                println!(
                    "{}: {}",
                    if end.ended { "ended" } else { "given up" },
                    end.done
                        .iter()
                        .map(|(step, outcome)| format!("{step}:{outcome}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            Ok(())
        }
        ["run", spec] => {
            let text = std::fs::read_to_string(spec).map_err(|err| format!("{spec}: {err}"))?;
            let spec: RunSpec =
                serde_json::from_str(&text).map_err(|err| format!("{spec}: {err}"))?;
            config.project(&spec.project).map_err(|err| err.to_string())?;
            let client = connect(&temporal.address, &temporal.namespace).await?;
            ensure_accounts(&client, &temporal.task_queue, temporal.accounts_check_secs).await?;
            let taken_up = spec.unit.is_some();
            let input = PipelineInput {
                unit: spec.unit.clone().unwrap_or_default(),
                project: spec.project.clone(),
                bead: spec.bead.clone(),
                flow_name: spec.flow_name.clone(),
                beat_secs: temporal.beat_secs,
                accounts: ACCOUNTS_ID.to_string(),
                within: spec.within.clone(),
                adopt: taken_up.then(|| spec.adopt.clone().unwrap_or_default()),
                stop_at_park: true,
                ..PipelineInput::default()
            };
            let id = spec.id.clone().unwrap_or_else(|| {
                format!(
                    "pipeline-{}-{}-{}",
                    spec.project,
                    spec.unit.as_deref().unwrap_or(&spec.bead),
                    factory_core_now()
                )
            });
            let mut options = pipeline_options(&temporal.task_queue, &id, &input, &spec.title);
            // Asked twice for the same pipeline, as after a restart of
            // whatever started it, this follows the one there is.
            options.id_reuse_policy = WorkflowIdReusePolicy::RejectDuplicate;
            match client.start_workflow(Pipeline::run, input, options).await {
                Ok(_) | Err(WorkflowStartError::AlreadyStarted { .. }) => {}
                Err(err) => return Err(format!("the pipeline could not be started: {err}")),
            }
            println!("workflow {id}");
            follow(&client.get_workflow_handle::<Pipeline>(id)).await
        }
        ["watch", pipeline] => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            let handle = client.get_workflow_handle::<Pipeline>(pipeline.to_string());
            println!("workflow {pipeline}");
            follow(&handle).await
        }
        ["standing", pipeline] => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            let standing = client
                .get_workflow_handle::<Pipeline>(pipeline.to_string())
                .query(Pipeline::standing, (), WorkflowQueryOptions::default())
                .await
                .map_err(|err| format!("the pipeline could not be asked: {err}"))?;
            println!("unit: {}", standing.unit);
            println!("step: {}", standing.step);
            if let Some(reason) = &standing.refused {
                println!("ended before its first step: {reason}");
            }
            if let Some(reason) = &standing.parked {
                println!("parked: {reason}");
            }
            if let Some(secs) = standing.limited_secs {
                println!("waiting {secs}s for a usage limit to lift");
            }
            for (step, outcome) in &standing.done {
                println!("  {step}: {outcome}");
            }
            Ok(())
        }
        ["go-on", pipeline, step, note @ ..] if !note.is_empty() => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            let decision = GoOn {
                from: step.to_string(),
                note: note.join(" "),
            };
            client
                .get_workflow_handle::<Pipeline>(pipeline.to_string())
                .signal(Pipeline::go_on, decision, WorkflowSignalOptions::default())
                .await
                .map_err(|err| format!("the pipeline could not be told: {err}"))?;
            println!("told {pipeline} to go on from `{step}`");
            Ok(())
        }
        ["give-up", pipeline] => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            client
                .get_workflow_handle::<Pipeline>(pipeline.to_string())
                .signal(Pipeline::give_up, (), WorkflowSignalOptions::default())
                .await
                .map_err(|err| format!("the pipeline could not be told: {err}"))?;
            println!("told {pipeline} to give up");
            Ok(())
        }
        ["accounts"] => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            let board = client
                .get_workflow_handle::<Accounts>(ACCOUNTS_ID.to_string())
                .query(Accounts::board, (), WorkflowQueryOptions::default())
                .await
                .map_err(|err| format!("the accounts could not be asked: {err}"))?;
            let now = factory_core_now();
            if board.checked_at == 0 {
                println!("not checked yet");
            } else {
                println!("checked {}s ago", now.saturating_sub(board.checked_at));
            }
            if let Some(trouble) = &board.trouble {
                println!("the last check failed: {trouble}");
            }
            let left = |name: &str, window: &Option<factory_claude::Window>| match window {
                Some(window) if window.resets_at > now => format!(
                    "{name} {:.0}% used, resets in {}m",
                    window.utilization * 100.0,
                    (window.resets_at - now) / 60
                ),
                _ => format!("{name} not open"),
            };
            let nothing = Default::default();
            for id in &board.picture.order {
                let state = board.picture.states.get(id).unwrap_or(&nothing);
                println!(
                    "{id}: {}; {}",
                    left("five-hour", &state.five_hour),
                    left("week", &state.seven_day)
                );
                for (scope, until) in &state.blocked {
                    if *until > now {
                        let what = if scope == "*" { "everything" } else { scope };
                        println!("  at a limit for {what}, for {}m more", (until - now) / 60 + 1);
                    }
                }
                if let Some(concern) = &state.concern {
                    println!("  LOOK AT THIS: {concern}");
                }
            }
            for waiting in &board.waiting {
                println!("waiting: {} (for `{}`)", waiting.who, waiting.model);
            }
            Ok(())
        }
        ["accounts", "recheck"] => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            client
                .get_workflow_handle::<Accounts>(ACCOUNTS_ID.to_string())
                .signal(Accounts::recheck, (), WorkflowSignalOptions::default())
                .await
                .map_err(|err| format!("the accounts could not be told: {err}"))?;
            println!("told the accounts to look again");
            Ok(())
        }
        _ => Err("usage: factory-engine worker | start <project> <bead> [--wait] | run <spec.json> | watch <pipeline> | standing <pipeline> | go-on <pipeline> <step> <note> | give-up <pipeline> | accounts [recheck]   [--config <dir>]".to_string()),
    }
}

fn factory_core_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match command(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("factory-engine: {reason}");
            ExitCode::from(1)
        }
    }
}
