//! Dev tooling for the factory: the quality command, the docs check, the gate
//! runner, and a self-test proving the first and third can say no.

mod check;
mod docs;
mod error;
mod gate;
mod release;
mod selftest;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use error::Error;

const USAGE: &str = "\
usage:
  cargo xtask check [--offline]             format check, clippy with warnings denied, every test
  cargo xtask docs                          glossary terms, doc links, the size of AGENTS.md
  cargo xtask selftest [check|gate] [--offline]
                                            the check and gate commands say no when they should
  cargo xtask gate --list                   every gate with its checks and tiers
  cargo xtask gate <id> [--tier <tier>]...  run a gate; tiers: hermetic, host, live, owner
  cargo xtask release-notes <version>       draft docs/releases/v<version>.md from the commits
  cargo xtask package --target <triple> [--zig]
                                            build one platform's archive into target/dist
  cargo xtask release <version> [--dry-run] check this commit can be <version>, then tag and push

gate exit codes: 0 passed, 1 failed, 2 incomplete (a tier was skipped)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("xtask: {err}");
            if matches!(err, Error::Usage(_)) {
                eprintln!("{USAGE}");
            }
            ExitCode::from(1)
        }
    }
}

/// The workspace root: the directory above this crate.
fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().unwrap_or(manifest_dir).to_path_buf()
}

fn run(args: &[String]) -> Result<u8, Error> {
    let root = workspace_root();
    let (command, rest) = args
        .split_first()
        .ok_or_else(|| Error::Usage("no command given".to_string()))?;
    match command.as_str() {
        "check" => {
            let offline = flags(rest, &["--offline"])?.contains("--offline");
            let report = check::run(&check::Options {
                root,
                offline,
                target_dir: None,
                quiet: false,
            })?;
            for stage in &report.failed {
                eprintln!("check: {stage} FAILED");
            }
            if report.passed() {
                eprintln!("check: ok");
            }
            Ok(u8::from(!report.passed()))
        }
        "docs" => {
            flags(rest, &[])?;
            let problems = docs::run(&root)?;
            for problem in &problems {
                eprintln!("docs: {problem}");
            }
            if problems.is_empty() {
                eprintln!("docs: ok");
            }
            Ok(u8::from(!problems.is_empty()))
        }
        "selftest" => {
            let (which, rest) = match rest.split_first() {
                Some((first, rest)) if !first.starts_with("--") => (first.as_str(), rest),
                _ => ("all", rest),
            };
            let offline = flags(rest, &["--offline"])?.contains("--offline");
            let mut outcomes = Vec::new();
            if matches!(which, "all" | "check") {
                outcomes.extend(selftest::check_cases(&root, offline)?);
            }
            if matches!(which, "all" | "gate") {
                outcomes.extend(selftest::gate_cases(&root)?);
            }
            if outcomes.is_empty() {
                return Err(Error::Usage(format!("unknown self-test `{which}`")));
            }
            let mut failed = false;
            for outcome in &outcomes {
                match &outcome.failure {
                    None => println!("ok    {}", outcome.name),
                    Some(reason) => {
                        failed = true;
                        println!("FAIL  {}: {reason}", outcome.name);
                    }
                }
            }
            Ok(u8::from(failed))
        }
        "gate" => gate_command(root, rest),
        "release-notes" => release::notes_command(&root, rest),
        "package" => release::package_command(&root, rest),
        "release" => release::release_command(&root, rest),
        other => Err(Error::Usage(format!("unknown command `{other}`"))),
    }
}

fn gate_command(root: PathBuf, args: &[String]) -> Result<u8, Error> {
    let mut id = None;
    let mut tiers = BTreeSet::new();
    let mut list = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--tier" => {
                let value = args
                    .next()
                    .ok_or_else(|| Error::Usage("--tier needs a value".to_string()))?;
                let tier = gate::Tier::parse(value)
                    .ok_or_else(|| Error::Usage(format!("unknown tier `{value}`")))?;
                tiers.insert(tier);
            }
            flag if flag.starts_with("--") => {
                return Err(Error::Usage(format!("unknown option `{flag}`")));
            }
            value if id.is_none() => id = Some(value.to_string()),
            extra => return Err(Error::Usage(format!("unexpected argument `{extra}`"))),
        }
    }
    if list {
        gate::print_list(&gate::list(&root)?);
        return Ok(0);
    }
    let id = id.ok_or_else(|| Error::Usage("no gate ID given".to_string()))?;
    let report = gate::run(&gate::Options {
        root,
        id,
        tiers,
        quiet: false,
    })?;
    gate::print_summary(&report);
    Ok(report.status.exit_code())
}

/// Accept only the listed flags, and return the ones given.
fn flags<'a>(args: &'a [String], allowed: &[&str]) -> Result<BTreeSet<&'a str>, Error> {
    let mut given = BTreeSet::new();
    for arg in args {
        if !allowed.contains(&arg.as_str()) {
            return Err(Error::Usage(format!("unexpected argument `{arg}`")));
        }
        given.insert(arg.as_str());
    }
    Ok(given)
}
