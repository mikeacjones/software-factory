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
use factory_driver::{Driver, DriverError, Options, Stepped};

use crate::accounts::{CheckCall, Picture};
use crate::steps::{
    NotPrepared, ParkCall, PrepareCall, Prepared, Started, StepCall, StepDone, StepRunner,
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
        .and_then(|mut driver| driver.run_step(&call.step, call.round));
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
        let options = self.options(call.flow_name.clone(), None);
        let driver = Driver::start(self.config.clone(), &call.project, &call.bead, &options)
            .map_err(|err| match err {
                // The bead itself is what is wrong, and stays wrong until
                // someone changes it.
                DriverError::NotLaunchable { .. } => NotPrepared::Refused(err.to_string()),
                other => NotPrepared::Failed(other.to_string()),
            })?;
        let unit = driver.unit().id.to_string();
        let pinned = self
            .unit_dir(&unit)
            .map_err(NotPrepared::Failed)?
            .join("flow.toml");
        let flow = std::fs::read_to_string(&pinned)
            .map_err(|err| NotPrepared::Failed(format!("{}: {err}", pinned.display())))?;
        Ok(Prepared { unit, flow })
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
