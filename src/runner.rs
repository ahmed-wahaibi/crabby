//! Runs one agent as a headless `claude` process.

use crate::catalog::Agent;
use serde_json::Value;
use std::{
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{self, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// Something the agent did, reported while it is still running.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A block of assistant text.
    Text(String),
    /// A tool call: the tool's name and its main argument (may be empty or multi-line).
    Tool { name: String, arg: String },
}

/// Runs `agent` on `task` in the current directory, hands each [`Event`] to
/// `on_event` as it arrives and returns the agent's final message.
///
/// The agent's file becomes extra system instructions, and only the tools named in
/// its header are pre-approved: a headless run cannot ask for permission, so any
/// other tool is refused.
pub fn run(
    agent: &Agent,
    task: &str,
    model: Option<&str>,
    effort: Option<&str>,
    mut on_event: impl FnMut(Event),
) -> io::Result<String> {
    let tools = agent.tools.join(",");
    // A `claude.cmd` install goes through cmd.exe, which cannot take a line break.
    let has_break = |s: &str| s.contains(['\n', '\r']);
    if has_break(&tools) || model.is_some_and(has_break) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the model name and the agent's tool list must not contain line breaks",
        ));
    }
    // In a file, not an argument: a large agent would exceed the Windows command-line limit.
    let prompt = TempFile::new(&agent.body)
        .map_err(|e| io::Error::new(e.kind(), format!("could not save the agent prompt: {e}")))?;
    let mut cmd = Command::new(claude_program());
    cmd.args(["--print", "--output-format", "stream-json", "--verbose"])
        .arg("--append-system-prompt-file")
        .arg(&prompt.0)
        .args(["--allowedTools", &tools]);
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    if let Some(e) = effort {
        cmd.args(["--effort", e]);
    }
    let mut result = None;
    feed(cmd, task, |line| {
        read_line(line, &mut on_event, &mut result)
    })
    .map_err(|e| match &result {
        Some(text) => io::Error::new(e.kind(), format!("{e}\n{text}")),
        None => e,
    })?;
    Ok(result.unwrap_or_default())
}

/// A file in the temp directory, deleted when dropped.
// shortcut: Drop does not run on Ctrl-C, so an interrupted run leaves one small file
// in the temp directory; add a signal handler if that ever matters.
struct TempFile(PathBuf);

impl TempFile {
    fn new(text: &str) -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = env::temp_dir().join(format!("crabby-{}-{nanos}.md", process::id()));
        // create_new: never write through a file or link someone else put there.
        let mut file = File::create_new(&path)?;
        let temp = Self(path);
        file.write_all(text.as_bytes())?;
        Ok(temp)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// What to spawn. `Command` on Windows only finds `.exe` files, but an npm install
/// of Claude Code is a `claude.cmd`.
fn claude_program() -> PathBuf {
    #[cfg(windows)]
    if let Some(found) = env::var_os("PATH").and_then(|p| find_on_path(&p)) {
        return found;
    }
    "claude".into()
}

/// The first `claude.exe` on `path`, else the first `claude.cmd`.
#[cfg(any(windows, test))]
fn find_on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    ["claude.exe", "claude.cmd"].iter().find_map(|name| {
        env::split_paths(path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// Handles one line of `claude --output-format stream-json`: events go to
/// `on_event`, the final message into `result`. Anything else is skipped.
fn read_line(line: &str, on_event: &mut impl FnMut(Event), result: &mut Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v["type"].as_str() {
        Some("assistant") => {
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                match (block["type"].as_str(), block["text"].as_str()) {
                    (Some("text"), Some(text)) => on_event(Event::Text(text.to_string())),
                    (Some("tool_use"), _) => on_event(Event::Tool {
                        name: block["name"].as_str().unwrap_or("?").to_string(),
                        arg: main_arg(&block["input"]),
                    }),
                    _ => {}
                }
            }
        }
        Some("result") => {
            if let Some(text) = v["result"].as_str() {
                *result = Some(text.to_string());
            }
        }
        _ => {}
    }
}

/// The argument that says most about a tool call: a well-known one, else the first string.
fn main_arg(input: &Value) -> String {
    const KEYS: [&str; 6] = [
        "file_path",
        "command",
        "pattern",
        "url",
        "query",
        "description",
    ];
    KEYS.iter()
        .find_map(|k| input[k].as_str())
        .or_else(|| input.as_object()?.values().find_map(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// Spawns `cmd`, sends it `task` on stdin and hands each line of its stdout to `on_line`.
fn feed(mut cmd: Command, task: &str, mut on_line: impl FnMut(&str)) -> io::Result<()> {
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
    let stdout = child.stdout.take().expect("stdout is piped");

    // Write on a second thread while this one drains stdout: writing a task larger
    // than the pipe buffer first would deadlock once the child's stdout pipe fills.
    let written = std::thread::scope(|s| {
        let writer = s.spawn(move || stdin.write_all(task.as_bytes())); // drop closes stdin

        // Bytes, not `lines()`: a line that is not UTF-8 must not end the run.
        for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
            on_line(&String::from_utf8_lossy(&line));
        }
        writer.join().expect("stdin writer does not panic")
    });
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!("`claude` exited with {status}")));
    }
    // Checked last: a child that died early breaks the pipe, and its exit status
    // above says more than "broken pipe" does.
    written.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not send the task to `claude`: {e}"),
        )
    })
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
        let mut lines = 0;
        feed(shell(echo), &task, |_| lines += 1).unwrap();
        assert_eq!(lines, 200_000);
    }

    #[test]
    fn a_child_that_exits_without_reading_reports_its_status() {
        let err = feed(shell("exit 3"), &"x".repeat(1_000_000), |_| {}).unwrap_err();
        assert!(err.to_string().contains("exited with"), "{err}");
    }

    #[test]
    fn stream_lines_become_events_and_bad_lines_are_skipped() {
        // Shapes as printed by claude 2.1.323, cut down to the fields that matter.
        let stream = r#"{"type":"system","subtype":"init","cwd":"C:\\x","tools":["Read"]}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Read","input":{"file_path":"C:\\x\\note.txt"},"caller":{"type":"direct"}}],"stop_reason":null},"parent_tool_use_id":null}
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01","type":"tool_result","content":"1\thello"}]}}
not json at all
{"type":"assistant","message":"wrong shape"}
{"type":"assistant","message":{"content":[{"type":"text"},{"type":"tool_use","input":{"limit":3,"other":"first string"}},42]}}
[1, 2
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"It says hello."}]},"parent_tool_use_id":null}
{"duration_ms":7568,"is_error":false,"subtype":"success","result":"It says hello.","type":"result"}"#;
        let (mut events, mut result) = (Vec::new(), None);
        for line in stream.lines() {
            read_line(line, &mut |e| events.push(e), &mut result);
        }
        let tool = |name: &str, arg: &str| Event::Tool {
            name: name.into(),
            arg: arg.into(),
        };
        assert_eq!(
            events,
            [
                tool("Read", r"C:\x\note.txt"),
                tool("?", "first string"),
                Event::Text("It says hello.".into()),
            ]
        );
        assert_eq!(result.as_deref(), Some("It says hello."));
    }

    #[test]
    fn the_prompt_file_is_removed_when_dropped() {
        let temp = TempFile::new("big prompt").unwrap();
        let path = temp.0.clone();
        assert_eq!(fs::read_to_string(&path).unwrap(), "big prompt");
        drop(temp);
        assert!(!path.exists());
    }

    #[test]
    fn claude_exe_is_preferred_and_claude_cmd_is_the_fallback() {
        let root = env::temp_dir().join(format!("crabby-path-test-{}", process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        for dir in [&a, &b] {
            fs::create_dir_all(dir).unwrap();
        }
        let path = env::join_paths([&a, &b]).unwrap();
        assert_eq!(find_on_path(&path), None);
        fs::write(a.join("claude.cmd"), "").unwrap();
        assert_eq!(find_on_path(&path), Some(a.join("claude.cmd")));
        fs::write(b.join("claude.exe"), "").unwrap();
        assert_eq!(find_on_path(&path), Some(b.join("claude.exe")));
        fs::remove_dir_all(&root).unwrap();
    }

    // Rust runs a `.cmd` through cmd.exe and rejects arguments it cannot escape,
    // so prove the kind of arguments `run` passes get through one.
    #[cfg(windows)]
    #[test]
    fn a_cmd_file_receives_the_arguments_and_the_task() {
        let dir = env::temp_dir().join(format!("crabby-cmd-test-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude.cmd");
        fs::write(&script, "@echo %*\r\n@findstr x*\r\n").unwrap();
        let mut cmd = Command::new(&script);
        cmd.args(["--allowedTools", "Read,Bash(git log:*)", "--file"])
            .arg(dir.join("a prompt.md"));
        let mut lines = Vec::new();
        let fed = feed(cmd, "the task\n", |l| lines.push(l.trim().to_string()));
        fs::remove_dir_all(&dir).unwrap();
        fed.unwrap();
        assert!(lines[0].contains("Read,Bash(git log:*)"), "{lines:?}");
        assert!(lines[0].contains("a prompt.md"), "{lines:?}");
        assert_eq!(lines[1], "the task");
    }
}
