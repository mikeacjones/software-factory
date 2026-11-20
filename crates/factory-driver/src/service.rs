//! Keeping the factory running: under launchd, as an agent of the desktop
//! session of the user it runs as.
//!
//! That session is where the login Keychain is unlocked, which the Claude
//! logins and the GitHub credential need, so the agent is limited to it.
//! launchd starts the service at login and again whenever it exits.
//!
//! What launchd runs is a *release*: a directory holding one build of the
//! factory together with the config that build was made to read. Releases
//! sit side by side and a link names the current one, so a deploy is one
//! rename and a build never reads a config written for another.

use std::path::{Path, PathBuf};

use crate::DriverError;

/// Where the releases are kept.
pub fn releases(state_dir: &Path) -> PathBuf {
    state_dir.join("releases")
}

/// The link to the release in use. launchd runs the factory through it.
pub fn current(state_dir: &Path) -> PathBuf {
    state_dir.join("current")
}

/// Where long runs are noted while they are alive, so that a service that
/// starts after one died can remove what that one left running.
pub fn live_runs(state_dir: &Path) -> PathBuf {
    state_dir.join("live-runs")
}

/// A release's name: when it was made, so that names sort by age, and the
/// commit it was built from if that is known.
pub fn release_name(now: u64, commit: Option<&str>) -> String {
    let commit = commit
        .map(|commit| {
            commit
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(12)
                .collect::<String>()
        })
        .filter(|commit| !commit.is_empty());
    match commit {
        Some(commit) => format!("{}-{commit}", factory_core::clock(now).replace(':', "")),
        None => factory_core::clock(now).replace(':', ""),
    }
}

use crate::io;

fn copy_tree(from: &Path, to: &Path) -> Result<(), DriverError> {
    std::fs::create_dir_all(to).map_err(io(to))?;
    for entry in std::fs::read_dir(from).map_err(io(from))? {
        let entry = entry.map_err(io(from))?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().map_err(io(&source))?.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(io(&source))?;
        }
    }
    Ok(())
}

/// Make a release from a build of the factory and a config directory, and
/// make it the current one. The link is replaced in one step, so there is
/// never a moment with no current release or with half of one. All but the
/// newest `keep` releases are removed, the one that was current among those
/// kept. Returns the release's directory.
///
/// A service that is running goes on with the release it started from: it
/// resolved the link when it started.
///
/// With a `signing` identity, each program of the release is signed with
/// it (`sign_program`), and a release whose programs could not be signed
/// is removed and never made current: it would run, but macOS would no
/// longer allow it what it allowed the release before.
pub fn install_release(
    state_dir: &Path,
    factory: &Path,
    config: &Path,
    name: &str,
    keep: usize,
    signing: Option<&str>,
) -> Result<PathBuf, DriverError> {
    let dir = releases(state_dir).join(name);
    if dir.exists() {
        return Err(DriverError::Preflight(format!(
            "the release {} already exists",
            dir.display()
        )));
    }
    let previous = std::fs::canonicalize(current(state_dir)).ok();
    copy_tree(config, &dir.join("config"))?;
    let binary = dir.join("factory");
    std::fs::copy(factory, &binary).map_err(io(factory))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .map_err(io(&binary))?;
    }
    let signed = |program: &Path| -> Result<(), DriverError> {
        let Some(identity) = signing else {
            return Ok(());
        };
        sign_program(program, identity).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&dir);
        })
    };
    signed(&binary)?;
    // The engine's command built beside the factory's goes with it, so
    // that a release is one build of both.
    let engine = factory.with_file_name("factory-engine");
    if engine.is_file() {
        let built = dir.join("factory-engine");
        put_program(&engine, &built)?;
        signed(&built)?;
        // A release whose engine cannot read its config would leave the
        // service starting and dying until someone looks: the engine of a
        // build older than the config, say, because only the factory's own
        // binary was rebuilt. It is refused before it becomes current.
        let checked = std::process::Command::new(&built)
            .arg("check")
            .arg("--config")
            .arg(dir.join("config"))
            .output()
            .map_err(io(&built))?;
        if !checked.status.success() {
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&checked.stdout),
                String::from_utf8_lossy(&checked.stderr)
            );
            let _ = std::fs::remove_dir_all(&dir);
            return Err(DriverError::Preflight(format!(
                "the engine built beside this factory cannot read this config, so the release was not made current: {}. Build every program of the workspace (`cargo build --release --locked --workspace --bins`) and deploy again",
                said.trim()
            )));
        }
    }
    make_current(state_dir, &dir)?;

    let mut all: Vec<PathBuf> = std::fs::read_dir(releases(state_dir))
        .map_err(io(&dir))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    all.sort();
    let spare = all.len().saturating_sub(keep.max(2));
    for old in all.into_iter().take(spare) {
        // The one a running service may still be reading stays.
        if Some(&old) != previous.as_ref() && old != dir {
            let _ = std::fs::remove_dir_all(&old);
        }
    }
    Ok(dir)
}

/// Point the link at a release, in one step: there is never a moment with
/// no current release.
fn make_current(state_dir: &Path, dir: &Path) -> Result<(), DriverError> {
    let link = current(state_dir);
    let staged = state_dir.join(format!("current.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(dir, &staged).map_err(io(&staged))?;
    std::fs::rename(&staged, &link).map_err(io(&link))
}

/// Make the release before the current one current again, and return it.
/// The release that was current stays on disk, so the rollback can itself
/// be undone by hand. A service that is running goes on with what it
/// started from until it is restarted.
pub fn rollback_release(state_dir: &Path) -> Result<PathBuf, DriverError> {
    let now = std::fs::canonicalize(current(state_dir)).map_err(|err| {
        DriverError::Preflight(format!(
            "there is no current release to roll back from ({}: {err})",
            current(state_dir).display()
        ))
    })?;
    let mut all: Vec<PathBuf> = std::fs::read_dir(releases(state_dir))
        .map_err(io(&releases(state_dir)))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    all.sort();
    let Some(at) = all.iter().position(|release| release == &now) else {
        return Err(DriverError::Preflight(format!(
            "the current release {} is not among the releases kept",
            now.display()
        )));
    };
    let Some(before) = at.checked_sub(1).map(|index| all[index].clone()) else {
        return Err(DriverError::Preflight(format!(
            "{} is the oldest release kept, so there is nothing to roll back to",
            now.display()
        )));
    };
    make_current(state_dir, &before)?;
    Ok(before)
}

/// Where the factory builds releases of its own landed commits: a worktree
/// of its repository kept from one deploy to the next, so that the build
/// starts from what the last one left.
pub fn deploy_checkout(state_dir: &Path, project: &str) -> PathBuf {
    state_dir.join("self-deploy").join(project)
}

/// A release the factory made of itself.
#[derive(Debug)]
pub struct Deployed {
    pub release: PathBuf,
    /// What the build printed.
    pub output: String,
    pub build_secs: u64,
}

/// Why the factory could not make a release of a commit that landed.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("the deploy checkout at {dir} could not be put at {commit}: {reason}")]
    Checkout {
        dir: PathBuf,
        commit: String,
        reason: String,
    },
    #[error("the build did not pass ({verdict})")]
    Build { verdict: String, output: String },
    #[error("the build passed but left no program at {0}")]
    NoBinary(PathBuf),
    #[error("{0}")]
    Release(String),
}

/// Build `commit` of the factory's repository in its deploy checkout with
/// the project's build command, and install what it built, with the
/// config at that commit, as the current release. Nothing is restarted:
/// the caller asks the service to stop at a safe point.
pub fn build_and_release(
    state_dir: &Path,
    repo: &Path,
    project: &str,
    commit: &str,
    build: &factory_config::CommandSpec,
    binary: &Path,
    service: &factory_config::ServiceSettings,
) -> Result<Deployed, DeployError> {
    let dir = deploy_checkout(state_dir, project);
    let checkout_failed = |reason: String| DeployError::Checkout {
        dir: dir.clone(),
        commit: commit.to_string(),
        reason,
    };
    if dir.join(".git").exists() {
        factory_git::checkout_detached(&dir, commit)
            .map_err(|err| checkout_failed(err.to_string()))?;
    } else {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|err| checkout_failed(err.to_string()))?;
        }
        // A worktree whose directory was removed is still known to git
        // until it is removed from git's list too.
        if factory_git::worktree_add_detached(repo, &dir, commit).is_err() {
            let _ = factory_git::worktree_remove(repo, &dir);
            let _ = std::fs::remove_dir_all(&dir);
            factory_git::worktree_add_detached(repo, &dir, commit)
                .map_err(|err| checkout_failed(err.to_string()))?;
        }
    }
    let result = factory_checks::run_command(
        &build.argv,
        &dir,
        std::time::Duration::from_secs(build.timeout_secs),
    );
    if result.verdict != factory_checks::Verdict::Passed {
        return Err(DeployError::Build {
            verdict: format!("{:?}", result.verdict),
            output: result.output,
        });
    }
    let program = dir.join(binary);
    if !program.is_file() {
        return Err(DeployError::NoBinary(program));
    }
    let release = install_release(
        state_dir,
        &program,
        &dir.join("config"),
        &release_name(factory_core::now_unix(), Some(commit)),
        service.releases_kept,
        service.signing_identity.as_deref(),
    )
    .map_err(|err| DeployError::Release(err.to_string()))?;
    Ok(Deployed {
        release,
        output: result.output,
        build_secs: result.duration.as_secs(),
    })
}

/// What a release's program is called to macOS, whatever its build: the
/// prefix and the program's file name, such as
/// `software-factory.factory-engine`.
const SIGNING_PREFIX: &str = "software-factory";

/// Sign a program of a release with `identity`, under a name that stays
/// the same from one build to the next, and check the signature. macOS
/// then knows each build as the same program (its designated requirement
/// is the name and the identity's certificate, not the build's hash), so
/// what it has allowed the program goes on applying after a deploy. The
/// identity's private key must let `codesign` use it without asking: a
/// deploy of the factory's own landed commit runs with nobody at the
/// screen, and a prompt would only time out.
fn sign_program(program: &Path, identity: &str) -> Result<(), DriverError> {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let identifier = format!("{SIGNING_PREFIX}.{name}");
    let path = program.to_string_lossy();
    let steps: [&[&str]; 2] = [
        &[
            "codesign",
            "--force",
            "--sign",
            identity,
            "--identifier",
            &identifier,
            &path,
        ],
        &["codesign", "--verify", "--strict", &path],
    ];
    for argv in steps {
        let done = run(argv)?;
        if !done.success() {
            let why = if done.timed_out {
                "it timed out, which is what a Keychain prompt nobody answers looks like: let codesign use the identity's key without asking".to_string()
            } else {
                format!("{}{}", done.stdout, done.stderr).trim().to_string()
            };
            return Err(DriverError::Preflight(format!(
                "`{}` on {} failed, so the release was not made current: {why}",
                argv[..2].join(" "),
                program.display()
            )));
        }
    }
    Ok(())
}

/// Copy a program to where it is run from, whole or not at all: it is
/// written beside its place and moved into it, so that a process already
/// running the old one goes on undisturbed.
fn put_program(from: &Path, to: &Path) -> Result<(), DriverError> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    let staged = to.with_extension(format!("new.{}", std::process::id()));
    std::fs::copy(from, &staged).map_err(io(from))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(io(&staged))?;
    std::fs::rename(&staged, to).map_err(io(to))
}

/// Put a release's build of the engine's command where the engine is run
/// from, unless that is the build already there. The command does not run
/// from the release itself: reaching the server from this machine is
/// allowed to a program by its path, so the path stays the same from one
/// release to the next. Returns whether anything was changed.
pub fn install_engine(release: &Path, engine: &Path) -> Result<bool, DriverError> {
    let built = release.join("factory-engine");
    if !built.is_file() {
        return Ok(false);
    }
    let same = std::fs::read(&built)
        .ok()
        .zip(std::fs::read(engine).ok())
        .is_some_and(|(new, old)| new == old);
    if same {
        return Ok(false);
    }
    put_program(&built, engine)?;
    Ok(true)
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launchd job that keeps the factory running.
///
/// It is an agent of the desktop (`Aqua`) session only: a copy started in
/// any other kind of session could not use the login Keychain. It is
/// started at login and again whenever it exits, which is also how it is
/// restarted on purpose: the service ends at a safe point and launchd
/// starts the current release.
///
/// launchd gives a job almost no environment, and none of what the user's
/// shell profile sets up: tools installed under the home directory are not
/// on its `PATH`. With `shell`, the user's login shell, the service is
/// started through it (`-lic`, so the profile and the interactive setup
/// both run) and it, and every agent and command it starts, has what the
/// user's own terminal has. `path` is what the shell itself starts from,
/// and all there is without one.
pub fn plist(label: &str, state_dir: &Path, path: &[String], shell: Option<&str>) -> String {
    let current = current(state_dir);
    let text = |value: &Path| xml(&value.to_string_lossy());
    // The shell replaces itself with the service, so launchd watches the
    // service's own process.
    let through_shell = shell
        .map(|shell| {
            format!(
                "        <string>{}</string>\n        <string>-lic</string>\n        <string>exec \"$@\"</string>\n        <string>{}</string>\n",
                xml(shell),
                xml(label)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{through_shell}        <string>{factory}</string>
        <string>serve</string>
        <string>--config</string>
        <string>{config}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{state}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>15</integer>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
        <key>LANG</key>
        <string>en_US.UTF-8</string>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = xml(label),
        factory = text(&current.join("factory")),
        config = text(&current.join("config")),
        state = text(state_dir),
        path = xml(&path.join(":")),
        log = text(&state_dir.join("serve.log")),
    )
}

fn run(argv: &[&str]) -> Result<factory_supervise::Captured, DriverError> {
    let spec = factory_supervise::Spec::new(
        argv.iter().copied(),
        std::env::temp_dir(),
        std::time::Duration::from_secs(60),
    );
    factory_supervise::capture(&spec).map_err(|err| DriverError::Preflight(err.to_string()))
}

/// Make a release from the factory that is running this code and a config
/// directory, named for now and for the commit the config's repository is
/// at, and make it current.
pub fn release_this_build(
    factory: &factory_config::Factory,
    config: &Path,
) -> Result<PathBuf, DriverError> {
    let build = std::env::current_exe().map_err(io(Path::new("the factory's own binary")))?;
    let commit = config
        .parent()
        .and_then(|repo| factory_git::rev_parse(repo, "HEAD").ok());
    install_release(
        &factory.state_dir,
        &build,
        config,
        &release_name(factory_core::now_unix(), commit.as_deref()),
        factory.service.releases_kept,
        factory.service.signing_identity.as_deref(),
    )
}

/// The factory's job in launchd: where it is described and what to call it.
#[derive(Clone, Debug)]
pub struct Job {
    pub label: String,
    /// The desktop session of this user: `gui/<uid>`.
    pub domain: String,
    pub plist: PathBuf,
}

impl Job {
    pub fn of(factory: &factory_config::Factory) -> Result<Job, DriverError> {
        let uid = run(&["/usr/bin/id", "-u"])?.stdout.trim().to_string();
        let home = std::env::var("HOME")
            .map_err(|_| DriverError::Preflight("HOME is not set".to_string()))?;
        if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
            return Err(DriverError::Preflight(
                "the user's ID could not be read".to_string(),
            ));
        }
        let label = factory.service.label.clone();
        Ok(Job {
            plist: Path::new(&home)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
            domain: format!("gui/{uid}"),
            label,
        })
    }

    fn target(&self) -> String {
        format!("{}/{}", self.domain, self.label)
    }

    /// How launchd sees the job now. A job it does not know has no state.
    pub fn state(&self) -> JobState {
        run(&["/bin/launchctl", "print", &self.target()])
            .map(|out| {
                if out.success() {
                    job_state(&out.stdout)
                } else {
                    JobState::default()
                }
            })
            .unwrap_or_default()
    }

    fn launchctl(&self, args: &[&str]) -> Result<(), DriverError> {
        let mut argv = vec!["/bin/launchctl"];
        argv.extend(args);
        let out = run(&argv)?;
        if out.success() {
            Ok(())
        } else {
            Err(DriverError::Preflight(format!(
                "`launchctl {}` failed: {}",
                args.join(" "),
                out.combined().trim()
            )))
        }
    }

    /// Describe the job to launchd and have it started. The desktop session
    /// must exist: the user is logged in at the machine.
    pub fn load(&self, plist_text: &str) -> Result<(), DriverError> {
        if let Some(dir) = self.plist.parent() {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        std::fs::write(&self.plist, plist_text).map_err(io(&self.plist))?;
        // A job already loaded under this name is replaced.
        let _ = self.launchctl(&["bootout", &self.target()]);
        let plist = self.plist.to_string_lossy().into_owned();
        self.launchctl(&["bootstrap", &self.domain, &plist])
    }

    /// Take the job out of launchd, which stops it, and remove its description.
    pub fn unload(&self) -> Result<(), DriverError> {
        let out = self.launchctl(&["bootout", &self.target()]);
        let _ = std::fs::remove_file(&self.plist);
        out
    }

    /// Start the job, or with `kill` stop it where it stands and start it again.
    pub fn kickstart(&self, kill: bool) -> Result<(), DriverError> {
        let target = self.target();
        if kill {
            self.launchctl(&["kickstart", "-k", &target])
        } else {
            self.launchctl(&["kickstart", &target])
        }
    }
}

/// The processes a process has started that are still running: for the
/// service, the steps in flight.
pub fn children(pid: u32) -> Vec<u32> {
    run(&["/usr/bin/pgrep", "-P", &pid.to_string()])
        .map(|out| {
            out.stdout
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A factory service that launchd did not start: one started by hand, which
/// would work beside the job on the same goals. `job` is the job's own
/// process, if it has one.
pub fn started_by_hand(job: Option<u32>) -> Vec<u32> {
    // The service as it is started, and the engine's worker it becomes
    // when everything runs on the engine.
    run(&[
        "/usr/bin/pgrep",
        "-f",
        "factory serve --config|bin/factory-engine worker --config",
    ])
    .map(|out| {
        out.stdout
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .filter(|pid| Some(*pid) != job && *pid != std::process::id())
            .collect()
    })
    .unwrap_or_default()
}

/// What `launchctl print` says about a job: whether it is running, under
/// which process, and how its last run ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobState {
    pub state: String,
    pub pid: Option<u32>,
    pub last_exit: Option<String>,
}

/// Read the lines of `launchctl print` that matter. Only the job's own
/// lines count: they are indented by one step, and the tables below them
/// hold other things named `state`.
pub fn job_state(printed: &str) -> JobState {
    let mut job = JobState::default();
    for line in printed.lines() {
        let own = line.starts_with('\t') && !line.starts_with("\t\t");
        let Some((key, value)) = line.trim().split_once(" = ") else {
            continue;
        };
        match (own, key) {
            (true, "state") => job.state = value.to_string(),
            (true, "pid") => job.pid = value.parse().ok(),
            (true, "last exit code") => job.last_exit = Some(value.to_string()),
            _ => {}
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_holds_a_build_with_its_config_and_becomes_current_in_one_step() {
        let root = std::env::temp_dir().join(format!("factory-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (state, source) = (root.join("state"), root.join("source"));
        std::fs::create_dir_all(source.join("config/roles")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(source.join("factory"), "build one").unwrap();
        std::fs::write(source.join("config/factory.toml"), "one").unwrap();
        std::fs::write(source.join("config/roles/builder.md"), "role one").unwrap();
        let install = |name: &str| {
            install_release(
                &state,
                &source.join("factory"),
                &source.join("config"),
                name,
                2,
                None,
            )
            .unwrap()
        };
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();

        let first = install("2026-11-15T130000Z-aaa");
        assert_eq!(
            std::fs::canonicalize(current(&state)).unwrap(),
            std::fs::canonicalize(&first).unwrap()
        );
        assert_eq!(read(current(&state).join("factory")), "build one");
        assert_eq!(
            read(current(&state).join("config/roles/builder.md")),
            "role one"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(first.join("factory"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the build can be run");
        }

        // The source moves on. A release is a snapshot: the one in use does
        // not change under a service that is reading it.
        std::fs::write(source.join("factory"), "build two").unwrap();
        std::fs::write(source.join("config/factory.toml"), "two").unwrap();
        assert_eq!(read(first.join("config/factory.toml")), "one");
        let second = install("2026-11-15T140000Z-bbb");
        assert_eq!(read(current(&state).join("factory")), "build two");
        assert_eq!(read(current(&state).join("config/factory.toml")), "two");
        assert_eq!(
            read(first.join("factory")),
            "build one",
            "the old release is whole"
        );
        assert!(
            install_release(
                &state,
                &source.join("factory"),
                &source.join("config"),
                "2026-11-15T140000Z-bbb",
                2,
                None
            )
            .is_err()
        );

        // Older releases go, but never the one that was current just now.
        let third = install("2026-11-15T150000Z-ccc");
        assert!(!first.exists() && second.exists() && third.exists());
        let fourth = install("2026-11-15T160000Z-ddd");
        assert!(!second.exists() && third.exists() && fourth.exists());
        assert!(
            !state
                .join(format!("current.{}", std::process::id()))
                .exists()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_factory_releases_what_landed_and_a_rollback_makes_the_one_before_current() {
        let root = std::env::temp_dir().join(format!("factory-self-deploy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (state, repo) = (root.join("state"), root.join("repo"));
        std::fs::create_dir_all(repo.join("config")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        // The "build" copies what the commit holds to where the release's
        // program is looked for.
        let commit = |text: &str| {
            std::fs::write(repo.join("program.txt"), text).unwrap();
            std::fs::write(repo.join("config/factory.toml"), text).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", text]);
            git(&["rev-parse", "HEAD"])
        };
        git(&["init", "-q", "-b", "main"]);
        // What the build leaves is ignored, as a cargo target directory is,
        // so it is kept from one deploy to the next.
        std::fs::write(repo.join(".gitignore"), "out/\n").unwrap();
        let one = commit("one");
        let build: factory_config::CommandSpec = toml::from_str(
            "argv = [\"sh\", \"-c\", \"mkdir -p out && cp program.txt out/factory && echo built\"]\ntimeout_secs = 60",
        )
        .unwrap();
        let binary = Path::new("out/factory");
        let deploy = |commit: &str, build: &factory_config::CommandSpec| {
            build_and_release(
                &state,
                &repo,
                "factory",
                commit,
                build,
                binary,
                &factory_config::ServiceSettings {
                    releases_kept: 3,
                    ..Default::default()
                },
            )
        };
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();

        let first = deploy(&one, &build).unwrap();
        assert!(first.output.contains("built"));
        assert_eq!(read(current(&state).join("factory")), "one");
        assert_eq!(read(current(&state).join("config/factory.toml")), "one");
        // Release names are to the second and sort by age.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let two = commit("two");
        let second = deploy(&two, &build).unwrap();
        assert_eq!(read(current(&state).join("factory")), "two");
        assert!(
            second.release.to_string_lossy().contains(&two[..12]),
            "a release is named for its commit"
        );

        // A build that fails, or leaves no program, changes nothing.
        let broken: factory_config::CommandSpec =
            toml::from_str("argv = [\"sh\", \"-c\", \"echo no; exit 3\"]\ntimeout_secs = 60")
                .unwrap();
        match deploy(&one, &broken) {
            Err(DeployError::Build { output, .. }) => assert!(output.contains("no")),
            other => panic!("{other:?}"),
        }
        let empty: factory_config::CommandSpec =
            toml::from_str("argv = [\"true\"]\ntimeout_secs = 60").unwrap();
        std::fs::remove_dir_all(deploy_checkout(&state, "factory").join("out")).unwrap();
        assert!(matches!(
            deploy(&one, &empty),
            Err(DeployError::NoBinary(_))
        ));
        assert_eq!(read(current(&state).join("factory")), "two");

        // The release before goes back in place, and the bad one is kept.
        let back = rollback_release(&state).unwrap();
        assert_eq!(back, std::fs::canonicalize(&first.release).unwrap());
        assert_eq!(read(current(&state).join("factory")), "one");
        assert!(second.release.exists());
        let err = rollback_release(&state).unwrap_err().to_string();
        assert!(err.contains("oldest release kept"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_release_whose_engine_cannot_read_its_config_never_becomes_current() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("factory-engine-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (state, source) = (root.join("state"), root.join("source"));
        std::fs::create_dir_all(source.join("config")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(source.join("factory"), "build").unwrap();
        std::fs::write(source.join("config/factory.toml"), "fine").unwrap();
        // The engine says whether it can read the config it is pointed at.
        let engine = source.join("factory-engine");
        std::fs::write(
            &engine,
            "#!/bin/sh
[ \"$1\" = check ] && [ \"$2\" = --config ] || exit 2
grep -q fine \"$3/factory.toml\" || { echo \"unknown field fallback_model\"; exit 1; }
",
        )
        .unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let install = |name: &str| {
            install_release(
                &state,
                &source.join("factory"),
                &source.join("config"),
                name,
                3,
                None,
            )
        };

        let good = install("2026-11-18T150000Z-aaa").unwrap();
        assert!(good.join("factory-engine").is_file());

        std::fs::write(source.join("config/factory.toml"), "new field").unwrap();
        let err = install("2026-11-18T160000Z-bbb").unwrap_err().to_string();
        assert!(
            err.contains("cannot read this config") && err.contains("unknown field fallback_model"),
            "{err}"
        );
        // The service goes on with the release it had, and the refused one
        // is gone.
        assert_eq!(
            std::fs::canonicalize(current(&state)).unwrap(),
            std::fs::canonicalize(&good).unwrap()
        );
        assert!(!releases(&state).join("2026-11-18T160000Z-bbb").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_launchd_job_runs_the_current_release_in_the_desktop_session() {
        let path = ["/opt/homebrew/bin".to_string(), "/usr/bin".to_string()];
        let state = Path::new("/Users/agent/.local/state/a&b");
        let text = plist("software-factory", state, &path, None);
        for expected in [
            "<key>Label</key>\n    <string>software-factory</string>",
            "<string>/Users/agent/.local/state/a&amp;b/current/factory</string>\n        <string>serve</string>\n        <string>--config</string>\n        <string>/Users/agent/.local/state/a&amp;b/current/config</string>",
            "<key>KeepAlive</key>\n    <true/>",
            "<key>RunAtLoad</key>\n    <true/>",
            "<key>LimitLoadToSessionType</key>\n    <string>Aqua</string>",
            "<key>PATH</key>\n        <string>/opt/homebrew/bin:/usr/bin</string>",
            "<key>StandardErrorPath</key>\n    <string>/Users/agent/.local/state/a&amp;b/serve.log</string>",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        // Through the user's login shell, the service has what the user's
        // terminal has. The shell runs its profile and then becomes the
        // service, so the process launchd watches is still the service.
        let through = plist("software-factory", state, &path, Some("/bin/zsh"));
        assert!(
            through.contains(
                "<array>\n        <string>/bin/zsh</string>\n        <string>-lic</string>\n        <string>exec \"$@\"</string>\n        <string>software-factory</string>\n        <string>/Users/agent/.local/state/a&amp;b/current/factory</string>\n        <string>serve</string>"
            ),
            "{through}"
        );
        assert!(!text.contains("-lic"));
        assert_eq!(
            release_name(1_794_747_600, Some("1d73d42\n")),
            "2026-11-15T130000Z-1d73d42"
        );
        assert_eq!(release_name(1_794_747_600, None), "2026-11-15T130000Z");
    }

    #[test]
    fn a_jobs_state_is_read_from_what_launchctl_prints() {
        let printed = "gui/501/software-factory = {\n\tactive count = 1\n\tpath = /Users/a/Library/LaunchAgents/software-factory.plist\n\tstate = running\n\n\tprogram = /x/current/factory\n\tpid = 4242\n\tlast exit code = 0\n\tendpoints = {\n\t\tstate = other\n\t\tpid = 1\n\t}\n}\n";
        assert_eq!(
            job_state(printed),
            JobState {
                state: "running".to_string(),
                pid: Some(4242),
                last_exit: Some("0".to_string()),
            }
        );
        assert_eq!(job_state("Could not find service"), JobState::default());
    }

    #[test]
    fn a_signed_release_names_each_program_the_same_from_build_to_build() {
        let root = std::env::temp_dir().join(format!("factory-signed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (state, source) = (root.join("state"), root.join("source"));
        std::fs::create_dir_all(source.join("config")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        // Programs codesign can sign, of our own: the engine answers `check`
        // with success. Not a copy of /usr/bin/true: that is Apple's arm64e
        // build, which macOS 15 refuses to run once it is signed as anyone
        // else's, so the engine's check failed there.
        for program in ["factory", "factory-engine"] {
            let path = source.join(program);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(source.join("config/factory.toml"), "fine").unwrap();
        let install = |name: &str, signing: Option<&str>| {
            install_release(
                &state,
                &source.join("factory"),
                &source.join("config"),
                name,
                3,
                signing,
            )
        };
        let identifier = |program: PathBuf| {
            let out = std::process::Command::new("codesign")
                .args(["--display", "--verbose=1"])
                .arg(&program)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .find_map(|line| line.strip_prefix("Identifier=").map(str::to_string))
                .unwrap_or_default()
        };

        // `-` is the ad-hoc identity: it needs no certificate, and is enough
        // to show the name each program is given.
        let signed = install("2026-11-20T150000Z-aaa", Some("-")).unwrap();
        assert_eq!(
            identifier(signed.join("factory")),
            "software-factory.factory"
        );
        assert_eq!(
            identifier(signed.join("factory-engine")),
            "software-factory.factory-engine"
        );

        // An identity the Keychain does not hold: the release is removed and
        // the one before stays current.
        let err = install(
            "2026-11-20T160000Z-bbb",
            Some("No Such Identity For The Factory Test"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("codesign --force") && err.contains("not made current"),
            "{err}"
        );
        assert_eq!(
            std::fs::canonicalize(current(&state)).unwrap(),
            std::fs::canonicalize(&signed).unwrap()
        );
        assert!(!releases(&state).join("2026-11-20T160000Z-bbb").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
