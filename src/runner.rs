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

    // The task goes over stdin so it is never mistaken for a flag value.
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("could not start `claude`: {e}")))?;
    child.stdin.take().expect("stdin is piped").write_all(task.as_bytes())?;

    let out = child.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        return Err(io::Error::other(format!("`claude` exited with {}\n{text}", out.status)));
    }
    Ok(text)
}
