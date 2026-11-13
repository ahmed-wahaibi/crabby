//! Runs one agent as a headless `claude` process.

use crate::catalog::Agent;
use serde_json::Value;
use std::{
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{self, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, PoisonError,
    },
    time::{Duration, Instant},
    time::{SystemTime, UNIX_EPOCH},
};

/// Something the agent did, reported while it is still running.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A block of assistant text.
    Text(String),
    /// A tool call: the tool's name and its main argument (may be empty or multi-line).
    Tool { name: String, arg: String },
    /// The id of the Claude Code session, from its first line; `--resume` takes it.
    Session(String),
}

/// What the final `result` event of a run reports. Fields the event lacks stay 0 / `None`.
#[derive(Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// Exact model ids that ran (from `modelUsage`), sorted.
    pub models: Vec<String>,
}

/// Runs `agent` on `task` in the current directory, hands each [`Event`] to
/// `on_event` as it arrives and returns the agent's final message and its usage
/// (`None` when no result event carried any).
///
/// The agent's file becomes extra system instructions, and only the tools named in
/// its header are pre-approved: a headless run cannot ask for permission, so any
/// other tool is refused.
pub fn run(
    agent: &Agent,
    task: &str,
    model: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
    cancel: &AtomicBool,
    on_event: impl FnMut(Event),
) -> io::Result<(String, Option<Usage>)> {
    let tools = agent.tools.join(",");
    // A `claude.cmd` install goes through cmd.exe, which cannot take a line break.
    let has_break = |s: &str| s.contains(['\n', '\r']);
    if has_break(&tools) || model.is_some_and(has_break) || resume.is_some_and(has_break) {
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
    if let Some(s) = resume {
        cmd.args(["--resume", s]);
    }
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    if let Some(e) = effort {
        cmd.args(["--effort", e]);
    }
    stream(cmd, task, cancel, on_event)
}

/// The system prompt of a plain conversation. Short on purpose: Claude Code's own is
/// about 40k tokens.
const CHAT_SYSTEM: &str = "You are Crabby, a coding assistant working in the user's current folder through a terminal. Be concise. Use the tools you have to look at the code before answering questions about it.";

/// The arguments of one turn of a plain conversation. `tools` is both the set of
/// tools offered and the set pre-approved: a headless session cannot ask.
fn chat_args(
    system_file: &std::path::Path,
    tools: &str,
    session: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = ["--print", "--output-format", "stream-json", "--verbose"]
        .map(String::from)
        .into();
    let mut add = |flag: &str, v: &str| a.extend([flag.to_string(), v.to_string()]);
    add("--system-prompt-file", &system_file.to_string_lossy());
    add("--tools", tools);
    add("--allowedTools", tools);
    // Slim set-up as in `ask`: no settings, plugins, skills or MCP servers in the prompt.
    add("--setting-sources", "");
    if let Some(s) = session {
        add("--resume", s);
    }
    if let Some(m) = model {
        add("--model", m);
    }
    if let Some(e) = effort {
        add("--effort", e);
    }
    for flag in ["--strict-mcp-config", "--disable-slash-commands"] {
        a.push(flag.into());
    }
    a
}

/// One turn of a plain conversation in the current directory. `session` is the id
/// from an earlier turn (see [`Event::Session`]), or `None` to start a conversation.
/// Returns the final message and usage.
pub fn chat(
    prompt: &str,
    tools: &str,
    session: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cancel: &AtomicBool,
    on_event: impl FnMut(Event),
) -> io::Result<(String, Option<Usage>)> {
    let has_break = |s: &str| s.contains(['\n', '\r']);
    if [Some(tools), session, model]
        .into_iter()
        .flatten()
        .any(has_break)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the model name, session id and tool list must not contain line breaks",
        ));
    }
    let file = TempFile::new(CHAT_SYSTEM)
        .map_err(|e| io::Error::new(e.kind(), format!("could not save the system prompt: {e}")))?;
    let mut cmd = Command::new(claude_program());
    cmd.args(chat_args(&file.0, tools, session, model, effort));
    stream(cmd, prompt, cancel, on_event)
}

/// One tool-less `claude` call with its own system prompt instead of Claude Code's
/// default (about 40k tokens). Returns the answer and its usage.
///
/// `--system-prompt` and `--tools ""` alone still leave plugins, skills and MCP
/// servers in the prompt, so the call also skips settings, slash commands and MCP.
pub fn ask(
    system: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    cancel: &AtomicBool,
) -> io::Result<(String, Option<Usage>)> {
    if model.is_some_and(|m| m.contains(['\n', '\r'])) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the model name must not contain line breaks",
        ));
    }
    let file = TempFile::new(system)
        .map_err(|e| io::Error::new(e.kind(), format!("could not save the system prompt: {e}")))?;
    let mut cmd = Command::new(claude_program());
    cmd.args(["--print", "--output-format", "stream-json", "--verbose"])
        .arg("--system-prompt-file")
        .arg(&file.0)
        .args([
            "--tools",
            "",
            "--no-session-persistence",
            "--disable-slash-commands",
        ])
        .args(["--strict-mcp-config", "--setting-sources", ""]);
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    if let Some(e) = effort {
        cmd.args(["--effort", e]);
    }
    stream(cmd, prompt, cancel, |_| {})
}

/// Feeds `task` to `cmd` and collects the final message and usage from its stream.
fn stream(
    cmd: Command,
    task: &str,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> io::Result<(String, Option<Usage>)> {
    let (mut result, mut usage) = (None, None);
    feed_with(cmd, task, cancel, IDLE_LIMIT, |line| {
        read_line(line, &mut on_event, &mut result, &mut usage)
    })
    .map_err(|e| match &result {
        Some(text) => io::Error::new(e.kind(), format!("{e}\n{text}")),
        None => e,
    })?;
    Ok((result.unwrap_or_default(), usage))
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
/// `on_event`, the final message into `result`, its usage into `usage`. Anything else
/// is skipped.
fn read_line(
    line: &str,
    on_event: &mut impl FnMut(Event),
    result: &mut Option<String>,
    usage: &mut Option<Usage>,
) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            if let Some(id) = v["session_id"].as_str() {
                on_event(Event::Session(id.to_string()));
            }
        }
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
            if let Some(u) = v["usage"].as_object() {
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                let mut models: Vec<String> = v["modelUsage"]
                    .as_object()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                models.sort();
                *usage = Some(Usage {
                    input_tokens: n("input_tokens"),
                    output_tokens: n("output_tokens"),
                    cache_read_tokens: n("cache_read_input_tokens"),
                    cache_write_tokens: n("cache_creation_input_tokens"),
                    cost_usd: v["total_cost_usd"].as_f64(),
                    duration_ms: v["duration_ms"].as_u64(),
                    models,
                });
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

/// [`feed_with`] that is never cancelled.
#[cfg(test)]
fn feed(cmd: Command, task: &str, on_line: impl FnMut(&str)) -> io::Result<()> {
    feed_with(
        cmd,
        task,
        &AtomicBool::new(false),
        Duration::from_secs(3600),
        on_line,
    )
}

/// Spawns `cmd`, sends it `task` on stdin and hands each line of its stdout to `on_line`.
/// Setting `cancel` kills the process (and its children on Windows) and makes this
/// return an `Interrupted` error. Its stderr is collected into the error, not shown.
fn feed_with(
    mut cmd: Command,
    task: &str,
    cancel: &AtomicBool,
    idle: Duration,
    mut on_line: impl FnMut(&str),
) -> io::Result<()> {
    // The task goes over stdin so it is never mistaken for a flag value.
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let pid = child.id();
    CHILDREN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(pid);
    let child = Mutex::new(child);
    let done = AtomicBool::new(false);
    let stalled = AtomicBool::new(false);
    let started = Instant::now();
    // Milliseconds since `started` at the last line of output.
    let last_line = AtomicU64::new(0);

    // Write on a second thread while this one drains stdout: writing a task larger
    // than the pipe buffer first would deadlock once the child's stdout pipe fills.
    let (written, err_text) = std::thread::scope(|s| {
        let writer = s.spawn(move || stdin.write_all(task.as_bytes())); // drop closes stdin
        let errors = s.spawn(move || {
            let mut text = String::new();
            let _ = io::Read::read_to_string(&mut stderr, &mut text);
            text
        });
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    kill_tree(&mut child.lock().expect("no panic holds the lock"), pid);
                    return;
                }
                let quiet =
                    started.elapsed().as_millis() as u64 - last_line.load(Ordering::Relaxed);
                if quiet > idle.as_millis() as u64 {
                    stalled.store(true, Ordering::Relaxed);
                    kill_tree(&mut child.lock().expect("no panic holds the lock"), pid);
                    return;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });

        // Bytes, not `lines()`: a line that is not UTF-8 must not end the run.
        for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
            on_line(&String::from_utf8_lossy(&line));
        }
        done.store(true, Ordering::Relaxed);
        (
            writer.join().expect("stdin writer does not panic"),
            errors.join().unwrap_or_default(),
        )
    });
    let status = child.into_inner().expect("no panic holds the lock").wait();
    CHILDREN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|p| *p != pid);
    let status = status?;
    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
    }
    if stalled.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "`claude` stopped responding for {} seconds and was stopped",
                idle.as_secs()
            ),
        ));
    }
    if !status.success() {
        let err = err_text.trim();
        let skip = err.chars().count().saturating_sub(500);
        let tail: String = err.chars().skip(skip).collect();
        return Err(io::Error::other(if tail.is_empty() {
            format!("`claude` exited with {status}")
        } else {
            format!("`claude` exited with {status}: {tail}")
        }));
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

/// Kills the process. On Windows a `claude.cmd` install runs `node` under `cmd.exe`,
/// which `Child::kill` alone would leave running, so the whole tree goes.
fn kill_tree(child: &mut process::Child, pid: u32) {
    kill_pid(pid);
    let _ = child.kill();
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(windows))]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

/// Process ids of the running `claude` children.
static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Stops every `claude` child still running, so none outlive a crash or an exit.
pub fn kill_all() {
    let pids = std::mem::take(&mut *CHILDREN.lock().unwrap_or_else(PoisonError::into_inner));
    for pid in pids {
        kill_pid(pid);
    }
}

/// How long `claude` may print nothing before it counts as not responding.
// shortcut: a fixed 15 minutes; a tool that runs silently for longer is stopped too.
const IDLE_LIMIT: Duration = Duration::from_secs(15 * 60);

/// `claude --version`, or `None` if it does not answer in three seconds.
pub fn claude_version() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = Command::new(claude_program())
            .arg("--version")
            .stdin(Stdio::null())
            .output();
        let _ = tx.send(
            out.ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
        );
    });
    rx.recv_timeout(Duration::from_secs(3))
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
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
        let (mut events, mut result, mut usage) = (Vec::new(), None, None);
        for line in stream.lines() {
            read_line(line, &mut |e| events.push(e), &mut result, &mut usage);
        }
        assert_eq!(usage, None); // that result event has no usage block
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
    fn usage_is_read_from_the_result_event() {
        // Captured from claude, cut down to the fields that matter.
        let real = r#"{"total_cost_usd":0.004165380000000001,"usage":{"input_tokens":2,"cache_creation_input_tokens":19796,"cache_read_input_tokens":16548,"output_tokens":81},"modelUsage":{"claude-haiku-5-5":{"inputTokens":2}},"is_error":false,"result":"Hi!","type":"result","duration_ms":3290}"#;
        let (mut result, mut usage) = (None, None);
        read_line(real, &mut |_| {}, &mut result, &mut usage);
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 2,
                output_tokens: 81,
                cache_read_tokens: 16548,
                cache_write_tokens: 19796,
                cost_usd: Some(0.004165380000000001),
                duration_ms: Some(3290),
                models: vec!["claude-haiku-5-5".into()],
            })
        );
        // Fields missing: zeros and `None`, no panic.
        usage = None;
        let sparse = r#"{"type":"result","result":"x","usage":{"output_tokens":5}}"#;
        read_line(sparse, &mut |_| {}, &mut result, &mut usage);
        assert_eq!(
            usage,
            Some(Usage {
                output_tokens: 5,
                ..Usage::default()
            })
        );
    }

    #[test]
    fn stream_init_line_gives_the_session_id() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        let mut events = vec![];
        read_line(line, &mut |e| events.push(e), &mut None, &mut None);
        assert_eq!(events, [Event::Session("abc-123".into())]);
    }

    #[test]
    fn chat_turns_resume_only_with_a_session_and_offer_only_their_tools() {
        let f = std::path::Path::new("sys.md");
        let first = chat_args(f, "Read,Grep", None, Some("haiku"), Some("low"));
        let later = chat_args(f, "Read,Grep", Some("sid-1"), None, None);
        assert!(!first.contains(&"--resume".to_string()));
        assert!(!first.contains(&"--no-session-persistence".to_string()));
        let pos = |a: &[String], f: &str| a.iter().position(|x| x == f);
        let i = pos(&later, "--resume").unwrap();
        assert_eq!(later[i + 1], "sid-1");
        for a in [&first, &later] {
            let t = pos(a, "--tools").unwrap();
            assert_eq!(a[t + 1], "Read,Grep");
            let t = pos(a, "--allowedTools").unwrap();
            assert_eq!(a[t + 1], "Read,Grep");
        }
        let m = pos(&first, "--model").unwrap();
        assert_eq!(first[m + 1], "haiku");
    }

    #[test]
    fn cancelling_kills_the_process_and_returns_at_once() {
        let sleeper = if cfg!(windows) {
            shell("ping -n 30 127.0.0.1 >nul")
        } else {
            shell("sleep 30")
        };
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let err = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, Ordering::Relaxed);
            });
            feed_with(sleeper, "", &cancel, Duration::from_secs(3600), |_| {}).unwrap_err()
        });
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_child_that_prints_nothing_for_too_long_is_stopped_as_not_responding() {
        let sleeper = if cfg!(windows) {
            shell("ping -n 30 127.0.0.1 >nul")
        } else {
            shell("sleep 30")
        };
        let started = Instant::now();
        let err = feed_with(
            sleeper,
            "",
            &AtomicBool::new(false),
            Duration::from_millis(300),
            |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn kill_all_stops_a_registered_child_and_forgets_it() {
        let mut child = if cfg!(windows) {
            shell("ping -n 30 127.0.0.1 >nul")
        } else {
            shell("sleep 30")
        }
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
        CHILDREN.lock().unwrap().push(child.id());
        kill_all();
        assert!(child.wait().is_ok_and(|s| !s.success()));
        assert!(!CHILDREN.lock().unwrap().contains(&child.id()));
    }

    // Live: spends two Haiku calls. `cargo test -- --ignored live_resume`.
    // A /run killed after its first reply, then resumed with `--resume`.
    #[test]
    #[ignore]
    fn live_resume_continues_a_killed_run() {
        let agent = Agent {
            name: "sc-live".into(),
            description: "d".into(),
            category: "c".into(),
            tools: vec!["Read".into()],
            body: "You are a terse test agent.".into(),
        };
        let cancel = AtomicBool::new(false);
        let mut session = None;
        let killed = run(
            &agent,
            "Remember the word PELICAN. Reply only: ok",
            Some("haiku"),
            Some("low"),
            None,
            &cancel,
            |e| match e {
                Event::Session(id) => session = Some(id),
                Event::Text(_) => cancel.store(true, Ordering::Relaxed),
                _ => {}
            },
        );
        eprintln!("first run ended: {killed:?}");
        let id = session.expect("a session id");
        let (second, usage) = run(
            &agent,
            "What word did I ask you to remember? Reply with the word only.",
            Some("haiku"),
            Some("low"),
            Some(&id),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        eprintln!(
            "resumed: {second}
{usage:?}"
        );
        assert!(second.to_uppercase().contains("PELICAN"), "{second}");
    }

    // Live: spends two Haiku calls. `cargo test -- --ignored live_chat`.
    #[test]
    #[ignore]
    fn live_chat_follow_up_continues_the_conversation() {
        let no = AtomicBool::new(false);
        let mut session = None;
        let (first, usage1) = chat(
            "Remember the word PINEAPPLE. Reply only: ok",
            "Read",
            None,
            Some("haiku"),
            Some("low"),
            &no,
            |e| {
                if let Event::Session(id) = e {
                    session = Some(id);
                }
            },
        )
        .unwrap();
        eprintln!("first: {first}\n{usage1:?}");
        let id = session.expect("a session id in the first reply");
        let (second, usage2) = chat(
            "What word did I ask you to remember? Reply with the word only.",
            "Read",
            Some(&id),
            Some("haiku"),
            Some("low"),
            &no,
            |_| {},
        )
        .unwrap();
        eprintln!("second: {second}\n{usage2:?}");
        assert!(second.to_uppercase().contains("PINEAPPLE"), "{second}");
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
