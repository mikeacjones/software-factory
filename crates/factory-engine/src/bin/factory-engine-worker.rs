//! A worker for the flow engine. Each step is done by running a command.
//!
//!   factory-engine-worker <address> <namespace> <task-queue> <deployment>
//!                         <build-id> <versioned|unversioned> <command>...
//!
//! With `FACTORY_DRAIN` set to a path, the worker stops at a safe point
//! once a file is there, as the factory's worker does at its drain flag.

use std::process::ExitCode;
use std::sync::Arc;

use factory_engine::steps::ExecRunner;
use factory_engine::worker::{WorkerSettings, run};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [
        address,
        namespace,
        task_queue,
        deployment,
        build_id,
        mode,
        command @ ..,
    ] = args.as_slice()
    else {
        eprintln!(
            "usage: factory-engine-worker <address> <namespace> <task-queue> <deployment> <build-id> <versioned|unversioned> <command>..."
        );
        return ExitCode::from(2);
    };
    let settings = WorkerSettings {
        address: address.clone(),
        namespace: namespace.clone(),
        task_queue: task_queue.clone(),
        deployment: deployment.clone(),
        build_id: build_id.clone(),
        versioned: mode == "versioned",
    };
    let runner = Arc::new(ExecRunner {
        argv: command.to_vec(),
        drain: std::env::var_os("FACTORY_DRAIN").map(std::path::PathBuf::from),
    });
    match run(settings, runner).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("factory-engine-worker: {reason}");
            ExitCode::from(1)
        }
    }
}
