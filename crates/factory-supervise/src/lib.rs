//! Run a child process under supervision: capture or stream its output,
//! enforce a wall-clock cap, and remove everything it started when it ends.
//!
//! This crate knows nothing about Claude. Liveness here is only the cap; the
//! stall rules of the full supervision library come later.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no command given")]
    EmptyCommand,
    #[error("could not run `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("lost contact with `{command}`: {source}")]
    Wait {
        command: String,
        source: std::io::Error,
    },
}

/// What to run and how.
#[derive(Clone, Debug)]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Variables to set, on top of this process's environment.
    pub env: Vec<(String, String)>,
    /// Variables to take out of the child's environment.
    pub env_remove: Vec<String>,
    /// Text to feed to stdin. The child sees end of input either way.
    pub stdin: Option<String>,
    pub timeout: Duration,
}

impl Spec {
    pub fn new<S: Into<String>>(
        argv: impl IntoIterator<Item = S>,
        cwd: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            env: Vec::new(),
            env_remove: Vec::new(),
            stdin: None,
            timeout,
        }
    }

    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// How a captured command ended.
#[derive(Clone, Debug)]
pub struct Captured {
    /// The exit code, if the process exited by itself.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// stdout and stderr, in that order.
    pub fn combined(&self) -> String {
        let mut text = self.stdout.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&self.stderr);
        text
    }
}

fn spawn(spec: &Spec, stdout: Stdio, stderr: Stdio) -> Result<Child, Error> {
    let (program, args) = spec.argv.split_first().ok_or(Error::EmptyCommand)?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&spec.cwd)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(stdout)
        .stderr(stderr)
        // Its own process group, so everything it starts can be removed with it.
        .process_group(0);
    for key in &spec.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|source| Error::Spawn {
        command: spec.display(),
        source,
    })?;
    if let (Some(text), Some(mut pipe)) = (spec.stdin.clone(), child.stdin.take()) {
        // Written from a thread so a child that reads slowly cannot block us.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    Ok(child)
}

/// SIGKILL a whole process group. Done through `kill` because this workspace
/// forbids unsafe code, which a direct system call would need.
fn kill_group(pgid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn read_all(mut source: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = source.read_to_end(&mut bytes);
        let _ = sender.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    receiver
}

/// Run a command to the end and return its output. A command still running at
/// its timeout is killed, with everything it started.
pub fn capture(spec: &Spec) -> Result<Captured, Error> {
    let started = Instant::now();
    let mut child = spawn(spec, Stdio::piped(), Stdio::piped())?;
    let pgid = child.id();
    let stdout = child.stdout.take().map(read_all);
    let stderr = child.stderr.take().map(read_all);

    let mut timed_out = false;
    let code = loop {
        match child.try_wait().map_err(|source| Error::Wait {
            command: spec.display(),
            source,
        })? {
            Some(status) => break status.code(),
            None if started.elapsed() >= spec.timeout => {
                timed_out = true;
                kill_group(pgid);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // Anything the command left running would keep the pipes open.
    kill_group(pgid);
    let collect = |receiver: Option<mpsc::Receiver<String>>| {
        receiver
            .and_then(|receiver| receiver.recv_timeout(Duration::from_secs(5)).ok())
            .unwrap_or_default()
    };
    Ok(Captured {
        code,
        timed_out,
        stdout: collect(stdout),
        stderr: collect(stderr),
        duration: started.elapsed(),
    })
}

/// What the caller wants after looking at the run so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    /// Kill the run now, for this reason.
    Kill(String),
}

/// How a streamed run ended.
#[derive(Clone, Debug)]
pub struct StreamExit {
    pub code: Option<i32>,
    pub timed_out: bool,
    /// Set if the caller asked for the kill.
    pub killed: Option<String>,
    pub duration: Duration,
    pub lines: u64,
}

/// Run a command, handing each line of its stdout to `on_line` as it arrives
/// and copying it to `log`. stderr goes to the same log. `tick` is called
/// about once a second and may ask for the run to be killed.
pub fn stream(
    spec: &Spec,
    log: &Path,
    on_line: &mut dyn FnMut(&str),
    tick: &mut dyn FnMut() -> Control,
) -> Result<StreamExit, Error> {
    let io = |source| Error::Io {
        path: log.to_path_buf(),
        source,
    };
    let mut log_file = File::create(log).map_err(io)?;
    let log_err = log_file.try_clone().map_err(io)?;

    let started = Instant::now();
    let mut child = spawn(spec, Stdio::piped(), Stdio::from(log_err))?;
    let pgid = child.id();
    let (sender, receiver) = mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    }

    let mut lines = 0;
    let mut timed_out = false;
    let mut killed = None;
    let mut last_tick = Instant::now();
    let mut handle = |line: String, log_file: &mut File| {
        let _ = writeln!(log_file, "{line}");
        on_line(&line);
    };
    let code = loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                lines += 1;
                handle(line, &mut log_file);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // stdout is closed: the process is ending or has ended.
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if let Some(status) = child.try_wait().map_err(|source| Error::Wait {
            command: spec.display(),
            source,
        })? {
            break status.code();
        }
        if started.elapsed() >= spec.timeout {
            timed_out = true;
        } else if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            if let Control::Kill(reason) = tick() {
                killed = Some(reason);
            }
        }
        if timed_out || killed.is_some() {
            kill_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
    };
    // Leave nothing behind, then read what was already in the pipe.
    kill_group(pgid);
    while let Ok(line) = receiver.recv_timeout(Duration::from_millis(200)) {
        lines += 1;
        handle(line, &mut log_file);
    }
    Ok(StreamExit {
        code,
        timed_out,
        killed,
        duration: started.elapsed(),
        lines,
    })
}

/// The last `max_lines` lines of a text, for a briefing or a message.
pub fn tail(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(max_lines);
    let mut out = String::new();
    if skip > 0 {
        out.push_str(&format!("[{skip} earlier lines left out]\n"));
    }
    out.push_str(&lines[skip..].join("\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout_ms: u64) -> Spec {
        Spec::new(
            ["sh", "-c", script],
            std::env::temp_dir(),
            Duration::from_millis(timeout_ms),
        )
    }

    #[test]
    fn capture_returns_output_and_exit_code() {
        let out = capture(&sh("echo out; echo err >&2; exit 3", 5000)).unwrap();
        assert_eq!(out.code, Some(3));
        assert!(!out.success());
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
    }

    #[test]
    fn capture_feeds_stdin() {
        let mut spec = sh("cat", 5000);
        spec.stdin = Some("fed".to_string());
        assert_eq!(capture(&spec).unwrap().stdout, "fed");
    }

    #[test]
    fn capture_kills_at_the_timeout() {
        let out = capture(&sh("sleep 30", 200)).unwrap();
        assert!(out.timed_out);
        assert_eq!(out.code, None);
        assert!(out.duration < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let spec = Spec::new(
            ["no-such-program-anywhere"],
            std::env::temp_dir(),
            Duration::from_secs(1),
        );
        assert!(matches!(capture(&spec), Err(Error::Spawn { .. })));
    }

    #[test]
    fn stream_hands_over_every_line_and_logs_it() {
        let log = std::env::temp_dir().join(format!("supervise-{}.log", std::process::id()));
        let mut seen = Vec::new();
        let exit = stream(
            &sh("echo one; echo two; echo three", 5000),
            &log,
            &mut |line| seen.push(line.to_string()),
            &mut || Control::Continue,
        )
        .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_file(&log).unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(seen, ["one", "two", "three"]);
        assert_eq!(logged, "one\ntwo\nthree\n");
    }

    #[test]
    fn stream_can_be_killed_by_the_caller() {
        let log = std::env::temp_dir().join(format!("supervise-kill-{}.log", std::process::id()));
        let exit = stream(
            &sh("echo started; sleep 30", 60_000),
            &log,
            &mut |_| {},
            &mut || Control::Kill("owner said stop".to_string()),
        )
        .unwrap();
        let _ = std::fs::remove_file(&log);
        assert_eq!(exit.killed.as_deref(), Some("owner said stop"));
        assert!(exit.duration < Duration::from_secs(10));
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("a\nb\nc", 2), "[1 earlier lines left out]\nb\nc");
        assert_eq!(tail("a\nb", 5), "a\nb");
    }
}
