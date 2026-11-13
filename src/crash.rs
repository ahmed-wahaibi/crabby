//! Crash reports: what a crash is, its signature, redaction, the report text, the
//! analysis prompt and filing as a GitHub issue. Everything is pure except `GhCli`,
//! `DryRun` and `Env::detect`; filing goes through the [`Gh`] trait so tests use a fake.

use crate::{catalog::Agent, runner};
use serde_json::Value;
use std::{
    cell::RefCell,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Where crash issues go.
pub const REPO: &str = "ahmed-wahaibi/crabby";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Crabby itself panicked.
    Panic,
    /// The `claude` process of a task failed underneath Crabby.
    Task,
}

/// What Crabby was doing. Never the task text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doing {
    pub task: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Crash {
    pub kind: Kind,
    /// `file:line` of a panic, or the child's exit status.
    pub location: String,
    pub message: String,
    /// A backtrace, or the tail of the child's error output.
    pub detail: Option<String>,
    pub doing: Option<Doing>,
}

static DOING: Mutex<Option<Doing>> = Mutex::new(None);

pub fn set_doing(d: Option<Doing>) {
    *DOING.lock().unwrap_or_else(PoisonError::into_inner) = d;
}

pub fn current_doing() -> Option<Doing> {
    DOING.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

thread_local! {
    /// What the panic hook saw on this thread: `(location, message, backtrace)`.
    pub static PANIC: RefCell<Option<(String, String, String)>> = const { RefCell::new(None) };
}

/// A panic caught in a job thread, from what the hook noted and the payload.
pub fn from_panic(payload: &(dyn std::any::Any + Send)) -> Crash {
    let text = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    let (location, message, backtrace) = PANIC.with(|p| p.borrow_mut().take()).unwrap_or((
        "unknown".into(),
        text.clone().unwrap_or_default(),
        String::new(),
    ));
    Crash {
        kind: Kind::Panic,
        location,
        message: if message.is_empty() {
            text.unwrap_or_default()
        } else {
            message
        },
        detail: (!backtrace.is_empty()).then_some(backtrace),
        doing: current_doing(),
    }
}

/// A backtrace without the frames of the panic machinery (hook, `panic_fmt`), which
/// would push the interesting frames out of the report.
pub fn trim_backtrace(bt: &str) -> String {
    const NOISE: [&str; 10] = [
        "std::backtrace",
        "crabby::crash_hook",
        "core::ops::function",
        "core::panic::",
        "std::panicking",
        "std::panic::",
        "alloc::boxed",
        "std::sys::backtrace",
        "core::panicking",
        "<alloc::boxed",
    ];
    let is_frame = |l: &str| {
        l.trim_start()
            .split_once(": ")
            .is_some_and(|(n, _)| n.bytes().all(|b| b.is_ascii_digit()))
    };
    let (mut out, mut skipping) = (vec![], true);
    for line in bt.lines() {
        if is_frame(line) {
            let sym = line.trim_start().split_once(": ").map_or("", |x| x.1);
            skipping = skipping && NOISE.iter().any(|n| sym.starts_with(n));
        }
        if !skipping {
            out.push(line);
        }
    }
    out.join(
        "
",
    )
}

/// What a failed task means.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// The user stopped it.
    Stopped,
    /// A known cause (not installed, signed out, rate limit, no network, usage): a
    /// clear message, never a crash report.
    Problem,
    Crash,
}

/// Sorts a failure of the `claude` process by its error kind and message.
pub fn classify(kind: io::ErrorKind, msg: &str) -> Failure {
    let m = msg.to_ascii_lowercase();
    const KNOWN: [&str; 17] = [
        "not logged in",
        "/login",
        "invalid api key",
        "authentication",
        "unauthorized",
        "401",
        "oauth",
        "rate limit",
        "429",
        "usage limit",
        "limit reached",
        "network",
        "econn",
        "enotfound",
        "could not resolve",
        "fetch failed",
        "no conversation found",
    ];
    match kind {
        io::ErrorKind::Interrupted => Failure::Stopped,
        io::ErrorKind::TimedOut => Failure::Crash,
        io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => Failure::Problem,
        _ if KNOWN.iter().any(|k| m.contains(k)) => Failure::Problem,
        _ if m.contains("exited with") => Failure::Crash,
        _ => Failure::Problem,
    }
}

/// The crash for a failed `claude` process: its exit status and the tail of its output.
pub fn from_failure(msg: &str, doing: Option<Doing>) -> Crash {
    let first = msg.lines().next().unwrap_or("");
    // `ExitStatus` prints as `exit code: 3` or `signal: 9`, so the status is two parts.
    let (location, message) = match first.split_once(" exited with ") {
        Some((_, rest)) => {
            let mut p = rest.splitn(3, ": ");
            let (a, b, tail) = (p.next().unwrap_or(""), p.next(), p.next().unwrap_or(""));
            (
                b.map_or(a.to_string(), |b| format!("{a}: {b}")),
                tail.to_string(),
            )
        }
        None => ("stopped responding".into(), first.to_string()),
    };
    Crash {
        kind: Kind::Task,
        location,
        message: if message.is_empty() {
            first.to_string()
        } else {
            message
        },
        detail: Some(msg.to_string()),
        doing,
    }
}

/// A message reduced to what stays the same between occurrences: first line, quoted
/// text and numbers blanked, lower case, at most 120 characters.
pub fn normalise(msg: &str) -> String {
    let mut out = String::new();
    let (mut quote, mut digits) = (None::<char>, false);
    for c in msg.lines().next().unwrap_or("").chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                out.push(c);
            }
            Some(_) => {}
            None if c == '"' || c == '`' => {
                quote = Some(c);
                out.push(c);
                out.push('…');
            }
            None if c.is_ascii_digit() => {
                if !digits {
                    out.push('N');
                }
                digits = true;
                continue;
            }
            None => out.push(c.to_ascii_lowercase()),
        }
        digits = false;
    }
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

/// A panic location without the machine-specific part: `src/ui.rs:12` for both
/// `C:\work\crabby\src\ui.rs:12` and `/home/x/crabby/src/ui.rs:12`.
fn stable_location(loc: &str) -> String {
    let l = loc.replace('\\', "/");
    match l.rfind("src/") {
        Some(i) => l[i..].to_string(),
        None => l,
    }
}

/// FNV-1a, because `DefaultHasher` may change between Rust versions.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

/// Identifies one distinct crash: kind, location (or exit status) and normalised message.
pub fn signature(c: &Crash) -> String {
    let kind = match c.kind {
        Kind::Panic => "panic",
        Kind::Task => "task",
    };
    let loc = match c.kind {
        Kind::Panic => stable_location(&c.location),
        Kind::Task => c.location.clone(),
    };
    format!(
        "{:016x}",
        fnv(&format!("{kind}|{loc}|{}", normalise(&c.message)))
    )
}

/// `Crash: index out of bounds in ui::transcript`.
pub fn title(c: &Crash) -> String {
    let msg: String = normalise(&c.message).chars().take(70).collect();
    match c.kind {
        Kind::Panic => {
            let mut file = stable_location(&c.location);
            // Drop the `:line:column` tail.
            while let Some((f, n)) = file.rsplit_once(':') {
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    break;
                }
                file = f.to_string();
            }
            let module = file
                .trim_start_matches("src/")
                .trim_end_matches(".rs")
                .replace('/', "::");
            format!("Crash: {msg} in {module}")
        }
        Kind::Task => format!("Crash: claude failed ({}): {msg}", c.location),
    }
}

/// Facts about this machine and build, for the report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Env {
    pub version: String,
    pub commit: String,
    pub claude: String,
    pub os: String,
    pub terminal: String,
}

impl Env {
    /// Asks `claude --version` (three seconds at most), so call it off the hot path.
    pub fn detect() -> Env {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let terminal = if var("WT_SESSION").is_some() {
            "Windows Terminal".to_string()
        } else {
            var("TERM_PROGRAM")
                .or_else(|| var("TERM"))
                .unwrap_or_else(|| "unknown".into())
        };
        Env {
            version: env!("CARGO_PKG_VERSION").into(),
            commit: option_env!("CRABBY_COMMIT").unwrap_or("unknown").into(),
            claude: runner::claude_version().unwrap_or_else(|| "unknown".into()),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            terminal,
        }
    }
}

/// The report, before redaction. Holds no task text, transcript or environment.
pub fn render(c: &Crash, env: &Env) -> String {
    let mut s = format!("<!-- crabby-signature: {} -->\n", signature(c));
    s += "## What happened\n\n";
    s += match c.kind {
        Kind::Panic => "Crabby panicked.\n",
        Kind::Task => "The `claude` process of a task failed.\n",
    };
    s += &format!(
        "\n```\n{}\n```\n\n## Where\n\n`{}`\n",
        cap(&c.message, 600),
        c.location
    );
    if let Some(d) = &c.detail {
        let label = match c.kind {
            Kind::Panic => "Backtrace",
            Kind::Task => "End of the error output",
        };
        s += &format!("\n{label}:\n\n```\n{}\n```\n", cap(d, 3000));
    }
    s += "\n## What Crabby was doing\n\n";
    match &c.doing {
        Some(d) => {
            let or = |v: &Option<String>| v.clone().unwrap_or_else(|| "default".into());
            s += &format!(
                "- task: {}\n- agent: {}\n- model: {}\n- effort: {}\n",
                d.task,
                d.agent.clone().unwrap_or_else(|| "none".into()),
                or(&d.model),
                or(&d.effort)
            );
        }
        None => s += "- no task was running\n",
    }
    s += &format!(
        "\n## Environment\n\n- Crabby {} ({})\n- Claude Code {}\n- OS: {}\n- terminal: {}\n\nSignature: `{}`\n",
        env.version,
        env.commit,
        env.claude,
        env.os,
        env.terminal,
        signature(c)
    );
    s
}

fn cap(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.trim().to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>().trim())
    }
}

/// The comment for a crash that already has an issue.
pub fn occurrence_comment(env: &Env) -> String {
    format!(
        "Another occurrence.\n\n- Crabby {} ({})\n- Claude Code {}\n- OS: {}\n- terminal: {}\n",
        env.version, env.commit, env.claude, env.os, env.terminal
    )
}

// ---------- redaction ----------

/// Case-insensitive replace of every `needle` in `text`.
fn replace_ci(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let (lower, n) = (text.to_lowercase(), needle.to_lowercase());
    // Lower-casing can change byte lengths for exotic text; then leave it alone.
    if lower.len() != text.len() {
        return text.to_string();
    }
    let (mut out, mut at) = (String::new(), 0);
    while let Some(i) = lower[at..].find(&n) {
        out += &text[at..at + i];
        out += with;
        at += i + n.len();
    }
    out + &text[at..]
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

fn secret_like(w: &str) -> bool {
    const PREFIXES: [&str; 11] = [
        "sk-",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "AIza",
        "eyJ",
    ];
    if PREFIXES.iter().any(|p| w.starts_with(p)) && w.len() >= 12 {
        return true;
    }
    w.len() >= 32
        && w.chars().any(|c| c.is_ascii_digit())
        && w.chars().any(|c| c.is_ascii_uppercase())
        && w.chars().any(|c| c.is_ascii_lowercase())
}

fn secret_name(w: &str) -> bool {
    let w = w.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "bearer",
    ]
    .iter()
    .any(|s| w.ends_with(s))
}

/// Removes what must not be posted publicly: the home folder and user name in paths,
/// anything that looks like a key or token, and the value after `KEY=`, `token:` and
/// `Bearer`. `home` and `user` are passed in so this is testable.
// shortcut: pattern based, so a secret with no known prefix, name or length is missed;
// the report is shown before it is sent, and the task text is never put in one.
pub fn redact(text: &str, home: Option<&str>, user: Option<&str>) -> String {
    let mut t = text.to_string();
    for h in home.into_iter().filter(|h| h.len() > 3) {
        let h = h.trim_end_matches(['/', '\\']);
        for form in [h.to_string(), h.replace('\\', "/"), h.replace('/', "\\")] {
            t = replace_ci(&t, &form, "~");
        }
        // JSON and Debug output double the backslashes.
        t = replace_ci(&t, &h.replace('\\', "\\\\"), "~");
    }
    if let Some(u) = user.filter(|u| u.len() >= 3) {
        t = replace_ci(&t, u, "<user>");
    }
    // Words and what lies between them; a secret word hides itself, and the value
    // after a secret's name.
    let (mut out, mut word, mut gap) = (String::new(), String::new(), String::new());
    let mut hide_next = false;
    let mut flush = |word: &mut String, gap: &mut String, out: &mut String, last: bool| {
        if !word.is_empty() {
            if hide_next || secret_like(word) {
                out.push_str("<redacted>");
                hide_next = false;
            } else {
                out.push_str(word);
            }
            if secret_name(word) && !last {
                hide_next = true;
            }
            word.clear();
        }
        // Only `=`, `:`, quotes and spaces may sit between a name and its value.
        if hide_next
            && !gap
                .chars()
                .all(|c| matches!(c, '=' | ':' | ' ' | '"' | '\''))
        {
            hide_next = false;
        }
        out.push_str(gap);
        gap.clear();
    };
    for c in t.chars() {
        if is_word(c) {
            if !gap.is_empty() {
                flush(&mut word, &mut gap, &mut out, false);
            }
            word.push(c);
        } else {
            if !word.is_empty() {
                flush(&mut word, &mut gap, &mut out, false);
            }
            gap.push(c);
        }
    }
    flush(&mut word, &mut gap, &mut out, true);
    out
}

/// [`redact`] with this machine's home folder and user name.
pub fn redact_here(text: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok();
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok();
    redact(text, home.as_deref(), user.as_deref())
}

// ---------- consent and filing ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consent {
    Ask,
    Always,
    Never,
}

impl Consent {
    pub fn parse(s: &str) -> Option<Consent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Consent::Ask),
            "always" => Some(Consent::Always),
            "never" => Some(Consent::Never),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Consent::Ask => "ask",
            Consent::Always => "always",
            Consent::Never => "never",
        }
    }
}

/// How a report's text is posted. One small seam, so tests never touch the network.
pub trait Gh {
    /// Open issues with the `crash` label, as `gh --json number,url,body` prints them.
    fn open_issues(&self) -> Result<String, String>;
    /// Opens an issue; returns its URL.
    fn create(&self, title: &str, body: &str) -> Result<String, String>;
    fn comment(&self, issue_url: &str, body: &str) -> Result<(), String>;
}

#[derive(Debug, PartialEq)]
pub enum Filed {
    Opened(String),
    Commented(String),
}

/// The URL of the open issue whose body carries `sig`.
pub fn find_existing(json: &str, sig: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    v.as_array()?
        .iter()
        .find(|i| i["body"].as_str().is_some_and(|b| b.contains(sig)))
        .and_then(|i| i["url"].as_str().map(String::from))
}

/// One issue per distinct crash: a comment on the open issue with the same signature,
/// else a new issue.
pub fn file(
    gh: &dyn Gh,
    sig: &str,
    title: &str,
    body: &str,
    comment: &str,
) -> Result<Filed, String> {
    match find_existing(&gh.open_issues()?, sig) {
        Some(url) => gh.comment(&url, comment).map(|()| Filed::Commented(url)),
        None => gh.create(title, body).map(Filed::Opened),
    }
}

/// The real thing: the `gh` CLI, each call limited to 30 seconds.
pub struct GhCli;

fn gh(args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                "the `gh` CLI is not installed".to_string()
            } else {
                format!("could not start `gh`: {e}")
            }
        })?;
    if let (Some(mut s), Some(text)) = (child.stdin.take(), stdin) {
        let _ = s.write_all(text.as_bytes());
    }
    let (mut out, mut err) = (
        child.stdout.take().expect("piped"),
        child.stderr.take().expect("piped"),
    );
    let read = |r: &mut dyn io::Read| {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if started.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                return Err("`gh` did not answer in 30 seconds (offline?)".into());
            }
        }
    };
    let (o, e) = (read(&mut out), read(&mut err));
    if status.success() {
        return Ok(o);
    }
    let e = e.trim();
    Err(
        if e.contains("gh auth login") || e.to_lowercase().contains("not logged") {
            "`gh` is not signed in (run `gh auth login`)".into()
        } else {
            format!("`gh` failed: {}", cap(e, 200))
        },
    )
}

impl Gh for GhCli {
    fn open_issues(&self) -> Result<String, String> {
        gh(
            &[
                "issue",
                "list",
                "--repo",
                REPO,
                "--label",
                "crash",
                "--state",
                "open",
                "--limit",
                "100",
                "--json",
                "number,url,body",
            ],
            None,
        )
    }

    fn create(&self, title: &str, body: &str) -> Result<String, String> {
        let base = [
            "issue",
            "create",
            "--repo",
            REPO,
            "--title",
            title,
            "--body-file",
            "-",
        ];
        let mut with_label = base.to_vec();
        with_label.extend(["--label", "crash"]);
        // The label may not exist in the repository yet; the issue is worth more.
        let out = gh(&with_label, Some(body)).or_else(|e| {
            if e.to_lowercase().contains("label") {
                gh(&base, Some(body))
            } else {
                Err(e)
            }
        })?;
        out.lines()
            .rev()
            .find(|l| l.starts_with("http"))
            .map(String::from)
            .ok_or_else(|| "`gh issue create` printed no URL".into())
    }

    fn comment(&self, url: &str, body: &str) -> Result<(), String> {
        gh(&["issue", "comment", url, "--body-file", "-"], Some(body)).map(|_| ())
    }
}

/// `CRABBY_FILING=dry-run`: nothing leaves the machine; issues and comments go to a
/// JSON file instead, so the whole flow can be tried.
pub struct DryRun(pub PathBuf);

impl DryRun {
    fn load(&self) -> Vec<Value> {
        fs::read_to_string(&self.0)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, v: &[Value]) -> Result<(), String> {
        fs::write(&self.0, serde_json::to_string_pretty(v).unwrap_or_default())
            .map_err(|e| e.to_string())
    }
}

impl Gh for DryRun {
    fn open_issues(&self) -> Result<String, String> {
        Ok(Value::Array(self.load()).to_string())
    }

    fn create(&self, title: &str, body: &str) -> Result<String, String> {
        let mut all = self.load();
        let url = format!("dry-run://issue/{}", all.len() + 1);
        all.push(serde_json::json!({"url": url, "title": title, "body": body, "comments": []}));
        self.save(&all)?;
        Ok(url)
    }

    fn comment(&self, url: &str, body: &str) -> Result<(), String> {
        let mut all = self.load();
        if let Some(i) = all.iter_mut().find(|i| i["url"] == url) {
            if let Some(c) = i["comments"].as_array_mut() {
                c.push(Value::String(body.into()));
            }
        }
        self.save(&all)
    }
}

// ---------- analysis ----------

/// The analysis is a guess and says so. Asked for at most 150 words.
pub const ANALYSIS_SYSTEM: &str = "You analyse crash reports of a Rust terminal app called Crabby. In at most 150 words give: Likely cause, and Proposed fix. Say what you are unsure of. Do not repeat the report.";

const SOURCE_TASK: &str = "Find the likely cause of this crash in the Crabby source in the current folder. Read only what you need; change nothing. Answer in at most 150 words: Likely cause, then Proposed fix, and say what you are unsure of.\n\n";

/// The Crabby checkout `start` lies in, if any: the nearest folder above it whose
/// `Cargo.toml` names the `crabby` package.
pub fn checkout_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|d| {
        let t = fs::read_to_string(d.join("Cargo.toml")).ok()?;
        t.lines()
            .any(|l| l.trim().replace(' ', "") == "name=\"crabby\"")
            .then(|| d.to_path_buf())
    })
}

/// One model call on the redacted report: a read-only debugging agent when inside a
/// checkout (`source`), else a tool-less call. Returns the analysis section, or `None`.
pub fn analyse(
    report: &str,
    source: Option<&[Agent]>,
    cancel: &AtomicBool,
) -> Option<(String, Option<runner::Usage>)> {
    let (text, usage) = match source.and_then(|a| a.iter().find(|a| a.name == "sc-debugger")) {
        Some(agent) => {
            let mut a = agent.clone();
            a.tools = ["Read", "Grep", "Glob"].map(String::from).into();
            runner::run(
                &a,
                &format!("{SOURCE_TASK}{report}"),
                Some("haiku"),
                Some("low"),
                None,
                cancel,
                |_| {},
            )
        }
        None => runner::ask(ANALYSIS_SYSTEM, report, Some("haiku"), Some("low"), cancel),
    }
    .ok()?;
    let text = redact_here(text.trim());
    (!text.is_empty()).then(|| {
        (
            format!(
                "\n## Analysis\n\n_Generated by a model from this report; a guess, not a fact._\n\n{text}\n"
            ),
            usage,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panic_at(loc: &str, msg: &str) -> Crash {
        Crash {
            kind: Kind::Panic,
            location: loc.into(),
            message: msg.into(),
            detail: None,
            doing: None,
        }
    }

    #[test]
    fn signature_ignores_numbers_machine_paths_and_quoted_text_but_not_the_place() {
        let a = panic_at(
            r"C:\work\crabby\src\ui.rs:12:5",
            "index out of bounds: the len is 3 but the index is 7",
        );
        let b = panic_at(
            "/home/x/crabby/src/ui.rs:12:5",
            "index out of bounds: the len is 10 but the index is 99",
        );
        assert_eq!(signature(&a), signature(&b));
        assert_ne!(
            signature(&a),
            signature(&panic_at("src/app.rs:12:5", &a.message))
        );
        assert_ne!(signature(&a), signature(&panic_at(&a.location, "other")));
        let q = |m: &str| signature(&panic_at("src/a.rs:1:1", m));
        assert_eq!(q("bad `foo`"), q("bad `bar`"));
        assert_eq!(normalise("Line 1\nline 2"), "line N");
        let task = |s: &str| from_failure(&format!("`claude` exited with {s}: boom 7"), None);
        assert_ne!(
            signature(&task("exit code: 1")),
            signature(&task("exit code: 2"))
        );
        assert_eq!(
            signature(&task("exit code: 1")),
            signature(&task("exit code: 1"))
        );
    }

    #[test]
    fn a_backtrace_starts_at_the_code_that_panicked() {
        let bt = "   0: std::backtrace::Backtrace::force_capture
             at x.rs:1
   1: crabby::crash_hook
   2: core::panicking::panic_fmt
   3: crabby::ui::draw
             at src/ui.rs:5
   4: std::panicking::try";
        assert_eq!(
            trim_backtrace(bt),
            "   3: crabby::ui::draw
             at src/ui.rs:5
   4: std::panicking::try"
        );
        assert_eq!(trim_backtrace("disabled"), "");
    }

    #[test]
    fn titles_name_the_crash() {
        let c = panic_at(
            r"C:\x\src\ui\transcript.rs:9:1",
            "index out of bounds: len 3",
        );
        assert_eq!(
            title(&c),
            "Crash: index out of bounds: len N in ui::transcript"
        );
        let t = from_failure("`claude` exited with exit code: 1: boom", None);
        assert_eq!(title(&t), "Crash: claude failed (exit code: 1): boom");
    }

    #[test]
    fn failures_are_sorted_into_stopped_known_problems_and_crashes() {
        use io::ErrorKind::*;
        assert_eq!(classify(Interrupted, "stopped"), Failure::Stopped);
        assert_eq!(
            classify(NotFound, "the `claude` command was not found"),
            Failure::Problem
        );
        assert_eq!(
            classify(Other, "`claude` exited with exit code: 1: Not logged in"),
            Failure::Problem
        );
        assert_eq!(
            classify(Other, "`claude` exited with exit code: 1: rate limit"),
            Failure::Problem
        );
        assert_eq!(
            classify(
                Other,
                "`claude` exited with exit code: 1: getaddrinfo ENOTFOUND"
            ),
            Failure::Problem
        );
        assert_eq!(
            classify(Other, "`claude` exited with exit code: 1: segfault"),
            Failure::Crash
        );
        assert_eq!(
            classify(Other, "`claude` exited with signal: 9"),
            Failure::Crash
        );
        assert_eq!(classify(TimedOut, "stopped responding"), Failure::Crash);
        assert_eq!(
            classify(Other, "could not save the system prompt: disk full"),
            Failure::Problem
        );
    }

    #[test]
    fn a_child_failure_keeps_its_status_and_error_tail() {
        let c = from_failure("`claude` exited with exit code: 3: bad thing", None);
        assert_eq!(
            (c.location.as_str(), c.message.as_str()),
            ("exit code: 3", "bad thing")
        );
        let h = from_failure("`claude` stopped responding for 15 minutes", None);
        assert_eq!(h.location, "stopped responding");
    }

    #[test]
    fn redaction_removes_the_home_path_user_name_keys_and_tokens() {
        let fake_key = "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef";
        let text = format!(
            "at C:\\Users\\JDoe\\Desktop\\crabby\\src\\ui.rs:1\n\
             at C:/Users/JDoe/x and /home/jdoe/y\n\
             {{\"p\":\"C:\\\\Users\\\\JDoe\\\\z\"}}\n\
             key {fake_key}\nANTHROPIC_API_KEY=abc123 token: \"hunter2\" Authorization: Bearer abcDEF123\n\
             gh ghp_0123456789abcdefABCDEF0123456789abcd done\n\
             long Zm9vYmFyMTIzNDU2Nzg5MEFCQ0RFRkdISUpLTE1OT1BRUlNU\n\
             fine words and src/ui.rs:12 stay"
        );
        let out = redact(&text, Some(r"C:\Users\JDoe"), Some("jdoe"));
        for gone in [
            "JDoe",
            "jdoe",
            fake_key,
            "abc123",
            "hunter2",
            "abcDEF123",
            "ghp_0123",
            "Zm9vYmFy",
        ] {
            assert!(!out.contains(gone), "{gone} still in:\n{out}");
        }
        assert!(out.contains("~\\Desktop\\crabby\\src\\ui.rs:1"), "{out}");
        assert!(out.contains("~/x and /home/<user>/y"), "{out}");
        assert!(out.contains("fine words and src/ui.rs:12 stay"), "{out}");
        assert!(out.contains("ANTHROPIC_API_KEY=<redacted>"), "{out}");
    }

    #[test]
    fn the_report_names_what_happened_but_never_the_task_text() {
        let mut c = panic_at("src/ui.rs:1:1", "boom");
        c.doing = Some(Doing {
            task: "run".into(),
            agent: Some("sc-x".into()),
            model: Some("haiku".into()),
            effort: None,
        });
        let env = Env {
            version: "0.1.0".into(),
            commit: "abc1234".into(),
            claude: "2.1.323".into(),
            os: "windows x86_64".into(),
            terminal: "Windows Terminal".into(),
        };
        let r = render(&c, &env);
        for part in [
            "boom",
            "src/ui.rs:1:1",
            "agent: sc-x",
            "model: haiku",
            "effort: default",
            "0.1.0 (abc1234)",
            "2.1.323",
            "windows x86_64",
            "Windows Terminal",
            &signature(&c),
        ] {
            assert!(r.contains(part), "{part}\n{r}");
        }
    }

    #[test]
    fn consent_words_parse() {
        assert_eq!(Consent::parse(" Always "), Some(Consent::Always));
        assert_eq!(Consent::parse("never"), Some(Consent::Never));
        assert_eq!(Consent::parse("maybe"), None);
        assert_eq!(Consent::Ask.name(), "ask");
    }

    /// Records what would be posted.
    #[derive(Default)]
    struct Fake {
        issues: Mutex<Vec<(String, String)>>,
        comments: Mutex<Vec<(String, String)>>,
        offline: bool,
    }

    impl Gh for Fake {
        fn open_issues(&self) -> Result<String, String> {
            if self.offline {
                return Err("offline".into());
            }
            let v: Vec<Value> = self
                .issues
                .lock()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, (_, b))| serde_json::json!({"url": format!("u{i}"), "body": b}))
                .collect();
            Ok(Value::Array(v).to_string())
        }
        fn create(&self, title: &str, body: &str) -> Result<String, String> {
            let mut i = self.issues.lock().unwrap();
            i.push((title.into(), body.into()));
            Ok(format!("u{}", i.len() - 1))
        }
        fn comment(&self, url: &str, body: &str) -> Result<(), String> {
            self.comments
                .lock()
                .unwrap()
                .push((url.into(), body.into()));
            Ok(())
        }
    }

    #[test]
    fn the_same_crash_twice_is_one_issue_and_one_comment() {
        let gh = Fake::default();
        let c = panic_at("src/ui.rs:1:1", "boom");
        let (sig, body) = (signature(&c), render(&c, &Env::default()));
        let go = || file(&gh, &sig, &title(&c), &body, "again");
        assert_eq!(go(), Ok(Filed::Opened("u0".into())));
        assert_eq!(go(), Ok(Filed::Commented("u0".into())));
        assert_eq!(gh.issues.lock().unwrap().len(), 1);
        assert_eq!(gh.comments.lock().unwrap().len(), 1);
        // A different crash is a new issue.
        let other = panic_at("src/app.rs:1:1", "boom");
        file(
            &gh,
            &signature(&other),
            "t",
            &render(&other, &Env::default()),
            "c",
        )
        .unwrap();
        assert_eq!(gh.issues.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_offline_filer_is_an_error_and_posts_nothing() {
        let gh = Fake {
            offline: true,
            ..Default::default()
        };
        assert!(file(&gh, "s", "t", "b", "c").is_err());
        assert!(gh.issues.lock().unwrap().is_empty());
    }

    #[test]
    fn the_dry_run_filer_keeps_issues_in_a_file_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("crabby-dry-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let gh = DryRun(dir.join("issues.json"));
        let _ = fs::remove_file(&gh.0);
        assert_eq!(
            file(&gh, "sig1", "t", "body sig1", "c").unwrap(),
            Filed::Opened("dry-run://issue/1".into())
        );
        assert_eq!(
            file(&gh, "sig1", "t", "body sig1", "c2").unwrap(),
            Filed::Commented("dry-run://issue/1".into())
        );
        let saved = fs::read_to_string(&gh.0).unwrap();
        assert!(saved.contains("c2"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_checkout_of_crabby_is_found_from_a_folder_inside_it() {
        let root = std::env::temp_dir().join(format!("crabby-co-{}", std::process::id()));
        let deep = root.join("src/x");
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"crabby\"\n").unwrap();
        assert_eq!(checkout_root(&deep), Some(root.clone()));
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"other\"\n").unwrap();
        assert!(checkout_root(&deep).is_none_or(|r| !r.starts_with(&root)));
        let _ = fs::remove_dir_all(root);
    }
}
