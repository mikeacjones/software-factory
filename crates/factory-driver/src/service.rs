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
pub fn install_release(
    state_dir: &Path,
    factory: &Path,
    config: &Path,
    name: &str,
    keep: usize,
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
    let link = current(state_dir);
    let staged = state_dir.join(format!("current.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(&dir, &staged).map_err(io(&staged))?;
    std::fs::rename(&staged, &link).map_err(io(&link))?;

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
/// starts the current release. `path` is the `PATH` its tools are found on,
/// since launchd gives a job almost none.
pub fn plist(label: &str, state_dir: &Path, path: &[String]) -> String {
    let current = current(state_dir);
    let text = |value: &Path| xml(&value.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{factory}</string>
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
    run(&["/usr/bin/pgrep", "-f", "factory serve --config"])
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
                2
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
    fn the_launchd_job_runs_the_current_release_in_the_desktop_session() {
        let text = plist(
            "software-factory",
            Path::new("/Users/agent/.local/state/a&b"),
            &["/opt/homebrew/bin".to_string(), "/usr/bin".to_string()],
        );
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
}
