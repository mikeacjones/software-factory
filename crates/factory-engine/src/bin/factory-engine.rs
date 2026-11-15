//! The flow engine's own command: its worker, and the calls that start a
//! pipeline and talk to one.
//!
//!   factory-engine worker                     run a worker: the engine and the steps
//!   factory-engine start <project> <bead>     start a pipeline for a bead
//!   factory-engine standing <pipeline>        where a pipeline stands
//!   factory-engine go-on <pipeline> <step> <note>
//!                                             continue a parked pipeline from a step
//!   factory-engine give-up <pipeline>         end a parked pipeline
//!
//!   --config <dir>   the factory's config directory, whose `[temporal]`
//!                    table says where the server is

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use factory_config::{ConfigDir, Temporal};
use factory_engine::engine::{GoOn, Pipeline, PipelineInput};
use factory_engine::factory::FactoryRunner;
use factory_engine::worker::{WorkerSettings, connect, run};
use temporalio_client::{
    WorkflowGetResultOptions, WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};

/// What this build of the engine is called: its release, when it runs
/// from one, so that a pipeline's history says which build ran each step.
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
            run(settings, runner).await
        }
        ["start", project, bead] => {
            // Refuse a project the factory does not know before anything
            // is started for it.
            config.project(project).map_err(|err| err.to_string())?;
            let client = connect(&temporal.address, &temporal.namespace).await?;
            let id = format!("pipeline-{project}-{bead}-{}", factory_core_now());
            let input = PipelineInput {
                project: project.to_string(),
                bead: bead.to_string(),
                beat_secs: temporal.beat_secs,
                ..PipelineInput::default()
            };
            let options = WorkflowStartOptions::new(temporal.task_queue.clone(), id.clone()).build();
            let handle = client
                .start_workflow(Pipeline::run, input, options)
                .await
                .map_err(|err| format!("the pipeline could not be started: {err}"))?;
            println!("{id}");
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
        ["standing", pipeline] => {
            let client = connect(&temporal.address, &temporal.namespace).await?;
            let standing = client
                .get_workflow_handle::<Pipeline>(pipeline.to_string())
                .query(Pipeline::standing, (), WorkflowQueryOptions::default())
                .await
                .map_err(|err| format!("the pipeline could not be asked: {err}"))?;
            println!("unit: {}", standing.unit);
            println!("step: {}", standing.step);
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
        _ => Err("usage: factory-engine worker | start <project> <bead> [--wait] | standing <pipeline> | go-on <pipeline> <step> <note> | give-up <pipeline>   [--config <dir>]".to_string()),
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
