//! `cargo xtask check`: the format check, clippy with warnings denied, and every test.

use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::error::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Format,
    Lint,
    Test,
}

impl Stage {
    pub const ALL: [Stage; 3] = [Stage::Format, Stage::Lint, Stage::Test];
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stage::Format => "format",
            Stage::Lint => "lint",
            Stage::Test => "test",
        })
    }
}

pub struct Options {
    /// The workspace to check.
    pub root: PathBuf,
    /// Never touch the network.
    pub offline: bool,
    /// Build somewhere other than the workspace's own target directory.
    pub target_dir: Option<PathBuf>,
    /// Discard the tools' output. Used by the self-test, whose fixtures fail on purpose.
    pub quiet: bool,
}

/// Which stages failed. Every stage runs even if an earlier one failed, so the
/// report says exactly what is wrong.
pub struct Report {
    pub failed: Vec<Stage>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.failed.is_empty()
    }
}

pub fn run(options: &Options) -> Result<Report, Error> {
    let mut failed = Vec::new();
    for stage in Stage::ALL {
        if !options.quiet {
            eprintln!("check: {stage}");
        }
        if !run_stage(stage, options)? {
            failed.push(stage);
        }
    }
    Ok(Report { failed })
}

fn run_stage(stage: Stage, options: &Options) -> Result<bool, Error> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut args: Vec<&str> = match stage {
        Stage::Format => vec!["fmt", "--all", "--check"],
        Stage::Lint => vec!["clippy", "--workspace", "--all-targets", "--locked"],
        Stage::Test => vec!["test", "--workspace", "--locked"],
    };
    if options.offline && stage != Stage::Format {
        args.push("--offline");
    }
    if stage == Stage::Lint {
        args.extend(["--", "-D", "warnings"]);
    }

    let mut command = Command::new(&cargo);
    command.args(&args).current_dir(&options.root);
    if let Some(target_dir) = &options.target_dir {
        command.env("CARGO_TARGET_DIR", target_dir);
    }
    if options.quiet {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = command.status().map_err(|source| Error::Spawn {
        command: format!("{cargo} {}", args.join(" ")),
        source,
    })?;
    Ok(status.success())
}
