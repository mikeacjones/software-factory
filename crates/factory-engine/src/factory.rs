//! The factory's own pipeline driver as the engine's step runner.
//!
//! The engine walks the flow. For each step it calls here, and the driver
//! does that one step of that unit exactly as it would in its own loop:
//! the worktree, the briefing, the run, the checks on the result, the
//! unit's record and its Slack thread are all the driver's. What the
//! driver no longer does for such a unit is decide what comes next.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use factory_config::ConfigDir;
use factory_driver::goal::{Event, RunningPipeline, Supervisor, Turned};
use factory_driver::{Driver, DriverError, IssueSweep, Options, Stepped};
use factory_slack::socket::Incoming;

use crate::accounts::{CheckCall, Picture};
use crate::steps::{
    AdoptCall, Adopted, NotPrepared, ParkCall, PrepareCall, Prepared, Started, StepCall, StepDone,
    StepRunner,
};

/// Runs steps with the factory's driver, against one config directory.
pub struct FactoryRunner {
    pub config: ConfigDir,
    /// Whether units post to Slack threads of their own.
    pub slack: bool,
}

impl FactoryRunner {
    fn options(&self, flow: Option<String>, note: Option<String>) -> Options {
        self.options_under(flow, note, None)
    }

    fn options_under(
        &self,
        flow: Option<String>,
        note: Option<String>,
        account: Option<String>,
    ) -> Options {
        Options {
            slack: self.slack,
            flow,
            within: None,
            continued: note,
            ruling: None,
            pausable: false,
            engine: true,
            account,
        }
    }

    fn unit_dir(&self, unit: &str) -> Result<PathBuf, String> {
        let factory = self.config.factory().map_err(|err| err.to_string())?;
        Ok(factory.state_dir.join("units").join(unit))
    }

    fn step(&self, call: &StepCall, started: Option<Started>) -> Result<StepDone, String> {
        // The driver writes a run's session ID to a file as soon as the
        // CLI reports it. Whoever is waiting for this step is told.
        let over = Arc::new(AtomicBool::new(false));
        if let Some(started) = started {
            let file = self
                .unit_dir(&call.unit)?
                .join(format!("in-flight-{}.session", call.step));
            let over = over.clone();
            std::thread::spawn(move || {
                while !over.load(Ordering::Relaxed) {
                    if let Ok(id) = std::fs::read_to_string(&file)
                        && !id.trim().is_empty()
                    {
                        started(id.trim());
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
        }
        let stepped = Driver::resume(
            self.config.clone(),
            &call.unit,
            &self.options_under(None, call.note.clone(), call.account.clone()),
            None,
        )
        .and_then(|mut driver| driver.run_step_at(&call.step, call.round, Some(&call.used)));
        over.store(true, Ordering::Relaxed);
        Ok(match stepped.map_err(|err| err.to_string())? {
            Stepped::Outcome(outcome) => StepDone {
                outcome,
                ..StepDone::default()
            },
            Stepped::Parked(reason) => StepDone {
                parked: Some(reason),
                ..StepDone::default()
            },
            Stepped::Limited { secs } => StepDone {
                limited_secs: Some(secs),
                ..StepDone::default()
            },
            // Not asked of a unit the engine walks; if it happens, the
            // step is simply run again.
            Stepped::Paused => return Err("the step was paused before it ran".to_string()),
        })
    }
}

impl StepRunner for FactoryRunner {
    fn prepare(&self, call: &PrepareCall) -> Result<Prepared, NotPrepared> {
        let mut options = self.options(call.flow_name.clone(), None);
        options.within = call.within.clone();
        let driver = Driver::start(self.config.clone(), &call.project, &call.bead, &options)
            .map_err(|err| match err {
                // The bead itself is what is wrong, and stays wrong until
                // someone changes it.
                DriverError::NotLaunchable { .. } => NotPrepared::Refused(err.to_string()),
                other => NotPrepared::Failed(other.to_string()),
            })?;
        let unit = driver.unit().id.to_string();
        let title = driver.unit().title.clone();
        let pinned = self
            .unit_dir(&unit)
            .map_err(NotPrepared::Failed)?
            .join("flow.toml");
        let flow = std::fs::read_to_string(&pinned)
            .map_err(|err| NotPrepared::Failed(format!("{}: {err}", pinned.display())))?;
        Ok(Prepared { unit, flow, title })
    }

    fn adopt(&self, call: &AdoptCall) -> Result<Adopted, NotPrepared> {
        // Taking a unit up is the driver's own continuing of it: the step
        // it goes on from, the note for that step and the ruling are put
        // on the unit's record, and its thread is told. What comes back
        // is where the record then says the unit stands.
        let options = Options {
            slack: self.slack,
            flow: call.flow_name.clone(),
            within: None,
            continued: call.note.clone(),
            ruling: call.ruling.clone(),
            pausable: false,
            engine: false,
            account: None,
        };
        let driver = Driver::resume(
            self.config.clone(),
            &call.unit,
            &options,
            call.from.as_deref(),
        )
        .map_err(|err| match err {
            // A unit that is not there, or has ended, stays so.
            DriverError::AlreadyEnded(_) | DriverError::Flow(_) => {
                NotPrepared::Refused(err.to_string())
            }
            other => NotPrepared::Failed(other.to_string()),
        })?;
        let unit = driver.unit();
        let pinned = self
            .unit_dir(&call.unit)
            .map_err(NotPrepared::Failed)?
            .join("flow.toml");
        let flow = std::fs::read_to_string(&pinned)
            .map_err(|err| NotPrepared::Failed(format!("{}: {err}", pinned.display())))?;
        Ok(Adopted {
            flow,
            position: unit.position.clone(),
            project: unit.project.to_string(),
            bead: unit.bead.clone(),
            title: unit.title.clone(),
        })
    }

    fn agent(
        &self,
        call: &StepCall,
        _session: Option<String>,
        started: Started,
    ) -> Result<StepDone, String> {
        // A run of this step that was cut off left its session on disk,
        // and the driver resumes it: there is nothing to pass in.
        self.step(call, Some(started))
    }

    fn plain(&self, call: &StepCall) -> Result<StepDone, String> {
        self.step(call, None)
    }

    fn park(&self, call: &ParkCall) -> Result<(), String> {
        Driver::resume(
            self.config.clone(),
            &call.unit,
            &self.options(None, None),
            None,
        )
        .and_then(|mut driver| driver.record_park(&call.reason))
        .map_err(|err| err.to_string())
    }

    fn finish(&self, unit: &str) -> Result<(), String> {
        Driver::resume(self.config.clone(), unit, &self.options(None, None), None)
            .and_then(|mut driver| driver.finish())
            .map_err(|err| err.to_string())
    }

    fn turn(&self, project: &str, event: &Event) -> Result<Turned, String> {
        // The supervisor for one turn reads the goals as their records
        // stand, applies the event, and leaves the records as the tiers
        // decided. It lives no longer than that.
        let mut supervisor = Supervisor::for_turn(self.config.clone(), Some(project))
            .map_err(|err| err.to_string())?;
        Ok(supervisor.turn(event.clone()))
    }

    fn listen(&self, heard: &dyn Fn(String, Incoming), stop: &AtomicBool) -> Result<(), String> {
        // Only for which channel is which project's.
        let supervisor =
            Supervisor::for_turn(self.config.clone(), None).map_err(|err| err.to_string())?;
        let route = |incoming: Incoming| {
            let channel = match &incoming {
                Incoming::Message { channel, .. } | Incoming::Action { channel, .. } => channel,
            };
            // What is said elsewhere is not for any project's tiers.
            if let Some(project) = supervisor.project_of(channel) {
                heard(project.to_string(), incoming);
            }
        };
        // What was kept when it could not be handed on, or when the
        // service that listened before stopped.
        for incoming in Supervisor::parked(&self.config) {
            route(incoming);
        }
        let Some(listener) = Supervisor::listener(&self.config).map_err(|err| err.to_string())?
        else {
            // A factory without Slack hears nothing.
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            return Ok(());
        };
        while !stop.load(Ordering::Relaxed) {
            match listener.incoming.recv_timeout(Duration::from_millis(250)) {
                Ok(incoming) => route(incoming),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the Slack listener stopped by itself".to_string());
                }
            }
        }
        listener.stop();
        // What arrived as it stopped.
        for incoming in listener.incoming.try_iter() {
            route(incoming);
        }
        Ok(())
    }

    fn keep(&self, incoming: &Incoming) {
        Supervisor::keep(&self.config, incoming);
    }

    fn running(&self) -> Result<Vec<RunningPipeline>, String> {
        Supervisor::running_pipelines(&self.config).map_err(|err| err.to_string())
    }

    fn issues(&self, project: &str) -> Result<IssueSweep, String> {
        let factory = self.config.factory().map_err(|err| err.to_string())?;
        let project = self
            .config
            .project(project)
            .map_err(|err| err.to_string())?;
        factory_driver::sweep_issues(&factory, &project, factory_driver::ISSUE_SWEEP_MIN_AGE_SECS)
            .map_err(|err| err.to_string())
    }

    fn draining(&self) -> bool {
        self.config
            .factory()
            .is_ok_and(|factory| factory_driver::draining(&factory.state_dir))
    }

    fn accounts(&self, call: &CheckCall) -> Result<Picture, String> {
        let factory = self.config.factory().map_err(|err| err.to_string())?;
        // Each account's login is used where it is kept, here, and only
        // what it has left goes back.
        let states =
            factory_driver::refresh_accounts(&factory, call.max_age_secs, call.forget_limits);
        Ok(Picture {
            order: factory
                .accounts
                .iter()
                .map(|account| account.id.clone())
                .collect(),
            states,
            grace_secs: factory.limit_grace_secs,
        })
    }
}
