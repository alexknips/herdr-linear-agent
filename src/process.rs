//! Child processes (git, `open`) under tokio, behind a trait so tests can
//! answer them.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::time::{Instant, sleep_until};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long output is still read after the child exited.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Duration,
}

impl Cmd {
    pub fn new(program: impl Into<String>, timeout: Duration) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            timeout,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The program and its arguments separated by spaces, for tests.
    #[cfg(test)]
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// The exit code; `None` when the child was killed.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }

    /// The first line of standard error, or how the child ended.
    pub fn error_text(&self) -> String {
        if self.timed_out {
            return "timed out".into();
        }
        match self.stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(line) => line.to_string(),
            None => match self.code {
                Some(code) => format!("exit code {code}"),
                None => "killed by a signal".into(),
            },
        }
    }
}

pub trait Runner: Send + Sync {
    fn run<'a>(&'a self, cmd: &'a Cmd) -> BoxFuture<'a, Result<Output>>;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run<'a>(&'a self, cmd: &'a Cmd) -> BoxFuture<'a, Result<Output>> {
        Box::pin(run(cmd))
    }
}

async fn run(cmd: &Cmd) -> Result<Output> {
    let mut child = tokio::process::Command::new(&cmd.program)
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not run `{}`", cmd.program))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        anyhow::bail!("`{}` has no output pipes", cmd.program);
    };
    let mut deadline = Instant::now() + cmd.timeout;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
    let (mut out_open, mut err_open) = (true, true);
    let mut status = None;
    // Both pipes are drained while waiting, so a chatty child cannot fill a
    // pipe and stall before its deadline.
    while status.is_none() || out_open || err_open {
        tokio::select! {
            read = stdout.read(&mut out_buf), if out_open => match read {
                Ok(n) if n > 0 => out.extend_from_slice(&out_buf[..n]),
                _ => out_open = false,
            },
            read = stderr.read(&mut err_buf), if err_open => match read {
                Ok(n) if n > 0 => err.extend_from_slice(&err_buf[..n]),
                _ => err_open = false,
            },
            waited = child.wait(), if status.is_none() => {
                status = Some(waited?);
                // A process the child left behind may keep the pipes open.
                deadline = deadline.min(Instant::now() + EXIT_DRAIN);
            }
            () = sleep_until(deadline), if status.is_some() => break,
            () = sleep_until(deadline), if status.is_none() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(Output {
                    code: None,
                    stdout: String::from_utf8_lossy(&out).into_owned(),
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                    timed_out: true,
                });
            }
        }
    }
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        timed_out: false,
    })
}

#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::*;

    /// Answers each command with the last rule whose needle its display line
    /// contains, so a test can override a default. Every command is recorded.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: Mutex<Vec<(String, Output)>>,
        pub calls: Mutex<Vec<Cmd>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn on(&self, needle: &str, output: Output) -> &Self {
            self.rules.lock().unwrap().push((needle.into(), output));
            self
        }

        pub fn lines(&self, needle: &str) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(Cmd::display)
                .filter(|line| line.contains(needle))
                .collect()
        }

        pub fn count(&self, needle: &str) -> usize {
            self.lines(needle).len()
        }
    }

    pub fn ok(stdout: &str) -> Output {
        Output {
            code: Some(0),
            stdout: stdout.into(),
            ..Output::default()
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stderr: stderr.into(),
            ..Output::default()
        }
    }

    impl Runner for FakeRunner {
        fn run<'a>(&'a self, cmd: &'a Cmd) -> BoxFuture<'a, Result<Output>> {
            self.calls.lock().unwrap().push(cmd.clone());
            let line = cmd.display();
            let answer = self
                .rules
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(needle, _)| line.contains(needle.as_str()))
                .map(|(_, output)| output.clone())
                .ok_or_else(|| anyhow::anyhow!("FakeRunner: no rule for `{line}`"));
            Box::pin(std::future::ready(answer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upper bound for work the runner must finish promptly; far above the
    /// deadlines under test, so a slow machine does not flake.
    const PROMPT: std::time::Duration = Duration::from_secs(5);

    fn sh(script: &str, timeout: Duration) -> Cmd {
        Cmd::new("sh", timeout).args(["-c", script])
    }

    async fn timed(cmd: Cmd) -> (Result<Output>, Duration) {
        let started = std::time::Instant::now();
        let result = RealRunner.run(&cmd).await;
        (result, started.elapsed())
    }

    #[tokio::test]
    async fn both_streams_and_the_exit_status_are_captured() {
        let script = "printf 'on stdout'; printf 'on stderr' >&2; exit 3";
        let (result, _) = timed(sh(script, PROMPT)).await;
        let output = result.unwrap();
        assert_eq!(
            output,
            Output {
                code: Some(3),
                stdout: "on stdout".into(),
                stderr: "on stderr".into(),
                timed_out: false,
            }
        );
        assert!(!output.success());
        assert_eq!(output.error_text(), "on stderr");

        let (result, _) = timed(sh("true", PROMPT)).await;
        assert!(result.unwrap().success());
    }

    #[test]
    fn error_text_prefers_stderr_then_says_how_the_child_ended() {
        let ended = |code, stderr: &str, timed_out| Output {
            code,
            stderr: stderr.into(),
            timed_out,
            ..Output::default()
        };
        let cases = [
            (
                ended(
                    Some(128),
                    "\n  fatal: not a git repository  \nhint\n",
                    false,
                ),
                "fatal: not a git repository",
            ),
            (ended(Some(2), "", false), "exit code 2"),
            (ended(None, "", false), "killed by a signal"),
            (ended(None, "partial output", true), "timed out"),
        ];
        for (output, expected) in cases {
            assert_eq!(output.error_text(), expected, "{output:?}");
        }
        let fetch = Cmd::new("git", PROMPT)
            .args(["-C", "/src/api", "fetch"])
            .arg("origin");
        assert_eq!(fetch.display(), "git -C /src/api fetch origin");
    }

    #[tokio::test]
    async fn a_background_grandchild_holding_the_pipes_does_not_delay_the_result() {
        let script = "sleep 30 & echo launched";
        let (result, took) = timed(sh(script, Duration::from_secs(20))).await;
        let output = result.unwrap();
        assert_eq!(output.code, Some(0));
        assert_eq!(output.stdout, "launched\n");
        assert!(!output.timed_out);
        assert!(took < PROMPT, "took {took:?}");
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_an_error() {
        let missing = Cmd::new("/nonexistent/herdr-linear-agent-no-such-program", PROMPT);
        let (result, _) = timed(missing).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("could not run"), "{error}");
    }

    #[tokio::test]
    async fn a_child_past_its_deadline_is_killed_even_while_it_floods_stdout() {
        let deadline = Duration::from_millis(300);
        for cmd in [
            Cmd::new("yes", deadline).arg("flood"),
            sh("sleep 30", deadline),
        ] {
            let shown = cmd.display();
            let (result, took) = timed(cmd).await;
            let output = result.unwrap();
            assert!(output.timed_out, "{shown}");
            assert_eq!(output.code, None, "{shown}");
            assert!(!output.success(), "{shown}");
            assert!(took < PROMPT, "{shown} took {took:?}");
        }
    }
}
