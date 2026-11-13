//! Runs one agent as a headless `claude` process.

use crate::catalog::Agent;
use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

/// Runs `agent` on `task` in the current directory and returns everything it printed.
///
/// The agent's file becomes extra system instructions, and only the tools named in
/// its header are pre-approved: a headless run cannot ask for permission, so any
/// other tool is refused.
pub fn run(agent: &Agent, task: &str, model: Option<&str>) -> io::Result<String> {
    let mut cmd = Command::new("claude");
    cmd.arg("--print")
        .args(["--append-system-prompt", &agent.body])
        .args(["--allowedTools", &agent.tools.join(",")]);
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    feed(cmd, task)
}

/// Spawns `cmd`, sends it `task` on stdin and collects its stdout.
fn feed(mut cmd: Command, task: &str) -> io::Result<String> {
    // The task goes over stdin so it is never mistaken for a flag value.
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            let msg = if e.kind() == io::ErrorKind::NotFound {
                "the `claude` command was not found on PATH; crabby runs agents through Claude Code, \
                 so install it (https://claude.com/claude-code) and check that `claude --version` works"
                    .to_string()
            } else {
                format!("could not start `claude`: {e}")
            };
            io::Error::new(e.kind(), msg)
        })?;
    let mut stdin = child.stdin.take().expect("stdin is piped");

    // Write on a second thread while this one drains stdout: writing a task larger
    // than the pipe buffer first would deadlock once the child's stdout pipe fills.
    let (written, out) = std::thread::scope(|s| {
        let writer = s.spawn(move || stdin.write_all(task.as_bytes())); // drop closes stdin
        let out = child.wait_with_output();
        (writer.join().expect("stdin writer does not panic"), out)
    });
    let out = out?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "`claude` exited with {}\n{text}",
            out.status
        )));
    }
    // Checked last: a child that died early breaks the pipe, and its exit status
    // above says more than "broken pipe" does.
    written.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not send the task to `claude`: {e}"),
        )
    })?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let (sh, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let mut cmd = Command::new(sh);
        cmd.args([flag, script]);
        cmd
    }

    #[test]
    fn a_task_larger_than_the_pipe_buffer_does_not_deadlock() {
        let task = "line\n".repeat(200_000); // about 1 MB each way
        let echo = if cfg!(windows) { "findstr x*" } else { "cat" };
        let out = feed(shell(echo), &task).unwrap();
        assert_eq!(out.lines().count(), 200_000);
    }

    #[test]
    fn a_child_that_exits_without_reading_reports_its_status() {
        let err = feed(shell("exit 3"), &"x".repeat(1_000_000)).unwrap_err();
        assert!(err.to_string().contains("exited with"), "{err}");
    }
}
