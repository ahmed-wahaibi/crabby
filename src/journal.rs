//! Files under `~/.crabby` (or `$CRABBY_HOME`): the task journal, the conversation
//! snapshot, crash reports and the settings that go with them. Every function that
//! decides something is pure and takes what it needs; `Store` only reads and writes.
//! A corrupt or partly written file is skipped, never a crash.

use crate::crash::{self, Consent, Crash, Env, Filed, Gh};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Finished entries kept, and crash reports kept.
pub const KEEP_FINISHED: usize = 50;
pub const KEEP_REPORTS: usize = 100;
/// Start-ups in a row that never reached a working interface before recovery is skipped.
pub const LOOP_LIMIT: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Finished,
    /// Ended by a known cause (not signed in, no network): the task is still wanted.
    Interrupted,
    Crashed,
}

impl TaskState {
    fn name(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::Finished => "finished",
            TaskState::Interrupted => "interrupted",
            TaskState::Crashed => "crashed",
        }
    }
}

/// One task, written before it starts. Only what is needed to resume or rerun it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    /// The process id of the Crabby window that owns it.
    pub owner: u32,
    /// `chat`, `run` or `plan`.
    pub kind: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub folder: String,
    pub input: String,
    /// The Claude session id, once known.
    pub session: Option<String>,
    /// The session a chat message continued when it started, for running it again.
    pub base_session: Option<String>,
    /// The entry this one resumes or reruns; it is dropped when this one finishes.
    pub replaces: Option<String>,
    pub state: TaskState,
    pub started: u64,
}

fn opt(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "owner": self.owner, "kind": self.kind, "agent": self.agent,
            "model": self.model, "effort": self.effort, "mode": self.mode,
            "folder": self.folder, "input": self.input, "session": self.session,
            "base_session": self.base_session, "replaces": self.replaces,
            "state": self.state.name(), "started": self.started,
        })
    }

    fn from_json(v: &Value) -> Option<Entry> {
        Some(Entry {
            id: opt(&v["id"])?,
            owner: u32::try_from(v["owner"].as_u64()?).ok()?,
            kind: opt(&v["kind"]).filter(|k| ["chat", "run", "plan"].contains(&k.as_str()))?,
            agent: opt(&v["agent"]),
            model: opt(&v["model"]),
            effort: opt(&v["effort"]),
            mode: opt(&v["mode"]),
            folder: opt(&v["folder"])?,
            input: opt(&v["input"])?,
            session: opt(&v["session"]),
            base_session: opt(&v["base_session"]),
            replaces: opt(&v["replaces"]),
            state: match v["state"].as_str()? {
                "running" => TaskState::Running,
                "finished" => TaskState::Finished,
                "interrupted" => TaskState::Interrupted,
                "crashed" => TaskState::Crashed,
                _ => return None,
            },
            started: v["started"].as_u64()?,
        })
    }
}

/// The conversation of one window: transcript, session, settings and the unsent input.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Snapshot {
    pub id: String,
    pub owner: u32,
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: String,
    pub session: Option<String>,
    pub input: String,
    /// `(kind, text)` with kind one of user, assistant, tool, info, error.
    pub entries: Vec<(String, String)>,
}

impl Snapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "owner": self.owner, "cwd": self.cwd, "model": self.model,
            "effort": self.effort, "mode": self.mode, "session": self.session,
            "input": self.input, "entries": self.entries,
        })
    }

    fn from_json(v: &Value) -> Option<Snapshot> {
        Some(Snapshot {
            id: opt(&v["id"])?,
            owner: u32::try_from(v["owner"].as_u64()?).ok()?,
            cwd: opt(&v["cwd"])?,
            model: opt(&v["model"]),
            effort: opt(&v["effort"]),
            mode: opt(&v["mode"])?,
            session: opt(&v["session"]),
            input: opt(&v["input"])?,
            entries: v["entries"]
                .as_array()?
                .iter()
                .map(|e| Some((opt(&e[0])?, opt(&e[1])?)))
                .collect::<Option<_>>()?,
        })
    }

    /// Worth offering back: something was said, or something was typed.
    pub fn has_content(&self) -> bool {
        !self.input.trim().is_empty() || self.entries.iter().any(|(k, _)| k == "user")
    }
}

// ---------- pure decisions ----------

/// The entries to offer: unfinished, owned by a window that is no longer running, and
/// not already replaced by a newer entry. Oldest first. Another live window's running
/// task is never listed.
pub fn interrupted(entries: &[Entry], me: u32, alive: &dyn Fn(u32) -> bool) -> Vec<Entry> {
    let replaced: Vec<&str> = entries
        .iter()
        .filter_map(|e| e.replaces.as_deref())
        .collect();
    let mut out: Vec<Entry> = entries
        .iter()
        .filter(|e| e.state != TaskState::Finished)
        .filter(|e| e.owner != me && !alive(e.owner))
        .filter(|e| !replaced.contains(&e.id.as_str()))
        .cloned()
        .collect();
    out.sort_by(|a, b| (a.started, &a.id).cmp(&(b.started, &b.id)));
    out
}

/// Ids of the finished entries to delete, keeping the newest `keep`.
pub fn prune_ids(entries: &[Entry], keep: usize) -> Vec<String> {
    let mut done: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.state == TaskState::Finished)
        .collect();
    done.sort_by(|a, b| (b.started, &b.id).cmp(&(a.started, &a.id)));
    done.into_iter().skip(keep).map(|e| e.id.clone()).collect()
}

/// What start-up should do about what it found.
#[derive(Debug, PartialEq)]
pub enum Plan {
    Nothing,
    /// Too many start-ups in a row failed: skip recovery and say so.
    Safe,
    Offer,
}

pub fn plan(startups_in_a_row: u32, has_conversation: bool, tasks: usize, reports: usize) -> Plan {
    if startups_in_a_row >= LOOP_LIMIT {
        Plan::Safe
    } else if has_conversation || tasks > 0 || reports > 0 {
        Plan::Offer
    } else {
        Plan::Nothing
    }
}

/// Whether the process is running. Pid reuse can make a dead window look alive, which
/// only means its tasks are not offered.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")));
    #[cfg(not(windows))]
    let out = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success());
    out.unwrap_or(true)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<seconds>-<pid>-<n>`: sorts by time and is unique across windows.
pub fn new_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:011}-{}-{}",
        now(),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// An entry for a task about to start, owned by this process.
pub fn new_entry(kind: &str, input: &str, folder: &str) -> Entry {
    Entry {
        id: new_id(),
        owner: std::process::id(),
        kind: kind.into(),
        agent: None,
        model: None,
        effort: None,
        mode: None,
        folder: folder.into(),
        input: input.into(),
        session: None,
        base_session: None,
        replaces: None,
        state: TaskState::Running,
        started: now(),
    }
}

// ---------- reports ----------

#[derive(Clone, Debug, PartialEq)]
pub struct ReportMeta {
    pub id: String,
    pub signature: String,
    pub title: String,
    /// `pending`, `filed` or `skipped`.
    pub status: String,
    pub issue: Option<String>,
    pub analysed: bool,
}

impl ReportMeta {
    fn to_json(&self) -> Value {
        json!({"id": self.id, "signature": self.signature, "title": self.title,
               "status": self.status, "issue": self.issue, "analysed": self.analysed})
    }

    fn from_json(v: &Value) -> Option<ReportMeta> {
        Some(ReportMeta {
            id: opt(&v["id"])?,
            signature: opt(&v["signature"])?,
            title: opt(&v["title"])?,
            status: opt(&v["status"])?,
            issue: opt(&v["issue"]),
            analysed: v["analysed"].as_bool().unwrap_or(false),
        })
    }
}

/// What `/crashes` shows.
pub fn list_text(reports: &[ReportMeta]) -> String {
    if reports.is_empty() {
        return "no crash reports".into();
    }
    reports
        .iter()
        .take(15)
        .map(|r| {
            let state = match (&r.issue, r.status.as_str()) {
                (Some(url), _) => url.clone(),
                (None, "skipped") => "not sent".into(),
                (None, _) => "not sent yet".into(),
            };
            format!("{}  {}  {}", r.id, r.title, state)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------- the store ----------

#[derive(Clone, Debug)]
pub struct Store {
    base: PathBuf,
}

/// What a start-up found left behind by earlier windows.
#[derive(Debug, Default)]
pub struct Found {
    pub conversation: Option<Snapshot>,
    pub tasks: Vec<Entry>,
}

impl Store {
    pub fn at(base: impl Into<PathBuf>) -> Store {
        Store { base: base.into() }
    }

    /// `$CRABBY_HOME`, else `~/.crabby`.
    pub fn default_base() -> Option<PathBuf> {
        std::env::var_os("CRABBY_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .or_else(|| std::env::var_os("HOME"))
                    .map(|h| PathBuf::from(h).join(".crabby"))
            })
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    fn journal(&self) -> PathBuf {
        self.base.join("journal")
    }

    fn crashes(&self) -> PathBuf {
        self.base.join("crashes")
    }

    /// Write to a temporary file, then rename, so a crash leaves the old file or the new.
    fn write(path: &Path, text: &str) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }

    fn read_json(path: &Path) -> Option<Value> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    /// The parsed files named `<prefix>*.json` in `dir`; unreadable ones are removed.
    fn load<T>(dir: &Path, prefix: &str, parse: impl Fn(&Value) -> Option<T>) -> Vec<T> {
        let mut out = vec![];
        for f in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = f.path();
            let name = f.file_name().to_string_lossy().to_string();
            if !name.starts_with(prefix) || !name.ends_with(".json") {
                continue;
            }
            match Self::read_json(&p).as_ref().and_then(&parse) {
                Some(v) => out.push(v),
                None => {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        out
    }

    pub fn save_entry(&self, e: &Entry) -> io::Result<()> {
        Self::write(
            &self.journal().join(format!("task-{}.json", e.id)),
            &e.to_json().to_string(),
        )
    }

    pub fn entries(&self) -> Vec<Entry> {
        Self::load(&self.journal(), "task-", Entry::from_json)
    }

    /// Updates one entry and, when it finished, drops the one it replaced.
    pub fn update_entry(&self, id: &str, f: impl FnOnce(&mut Entry)) -> io::Result<()> {
        let path = self.journal().join(format!("task-{id}.json"));
        let mut e = Self::read_json(&path)
            .as_ref()
            .and_then(Entry::from_json)
            .ok_or_else(|| io::Error::other("no such journal entry"))?;
        f(&mut e);
        self.save_entry(&e)?;
        if let (TaskState::Finished, Some(old)) = (e.state, &e.replaces) {
            self.forget_except(old, &e.id);
        }
        Ok(())
    }

    /// Deletes a task entry, and any entry that was started to replace it.
    pub fn forget(&self, id: &str) {
        self.forget_except(id, "");
    }

    /// [`Store::forget`], sparing the entry `keep`.
    fn forget_except(&self, id: &str, keep: &str) {
        let _ = fs::remove_file(self.journal().join(format!("task-{id}.json")));
        let others = self.entries();
        for e in others
            .iter()
            .filter(|e| e.replaces.as_deref() == Some(id) && e.id != keep)
        {
            let _ = fs::remove_file(self.journal().join(format!("task-{}.json", e.id)));
        }
    }

    pub fn save_snapshot(&self, s: &Snapshot) -> io::Result<()> {
        Self::write(
            &self.journal().join(format!("chat-{}.json", s.id)),
            &s.to_json().to_string(),
        )
    }

    pub fn forget_snapshot(&self, id: &str) {
        let _ = fs::remove_file(self.journal().join(format!("chat-{id}.json")));
    }

    /// What earlier windows left: the newest conversation and the tasks to offer.
    /// Older dead conversations are dropped, finished entries pruned.
    pub fn scan(&self, me: u32, alive: &dyn Fn(u32) -> bool) -> Found {
        let all = self.entries();
        for id in prune_ids(&all, KEEP_FINISHED) {
            self.forget(&id);
        }
        let mut chats: Vec<Snapshot> = Self::load(&self.journal(), "chat-", Snapshot::from_json)
            .into_iter()
            .filter(|s| s.owner != me && !alive(s.owner))
            .collect();
        chats.sort_by(|a, b| a.id.cmp(&b.id));
        let conversation = chats.pop();
        for s in chats {
            self.forget_snapshot(&s.id);
        }
        let conversation = conversation.filter(|s| {
            let keep = s.has_content();
            if !keep {
                self.forget_snapshot(&s.id);
            }
            keep
        });
        Found {
            conversation,
            tasks: interrupted(&all, me, alive),
        }
    }

    fn state_value(&self) -> Value {
        Self::read_json(&self.journal().join("state.json")).unwrap_or(json!({}))
    }

    fn set_state(&self, key: &str, v: Value) -> io::Result<()> {
        let mut s = self.state_value();
        s[key] = v;
        Self::write(&self.journal().join("state.json"), &s.to_string())
    }

    pub fn consent(&self) -> Consent {
        self.state_value()["consent"]
            .as_str()
            .and_then(Consent::parse)
            .unwrap_or(Consent::Ask)
    }

    pub fn set_consent(&self, c: Consent) -> io::Result<()> {
        self.set_state("consent", json!(c.name()))
    }

    /// Start-ups that have not yet reached a working interface (including this one).
    pub fn bump_startups(&self) -> u32 {
        let n = self.state_value()["startups"].as_u64().unwrap_or(0) as u32 + 1;
        let _ = self.set_state("startups", json!(n));
        n
    }

    pub fn clear_startups(&self) {
        let _ = self.set_state("startups", json!(0));
    }

    // ----- crash reports -----

    /// Writes the redacted report and its metadata; returns the id. Keeps the last 100.
    pub fn save_report(&self, c: &Crash, env: &Env) -> io::Result<String> {
        let id = new_id();
        let meta = ReportMeta {
            id: id.clone(),
            signature: crash::signature(c),
            title: crash::redact_here(&crash::title(c)),
            status: "pending".into(),
            issue: None,
            analysed: false,
        };
        let dir = self.crashes();
        // The body first: a meta file without a body would be useless.
        Self::write(
            &dir.join(format!("{id}.md")),
            &crash::redact_here(&crash::render(c, env)),
        )?;
        Self::write(&dir.join(format!("{id}.json")), &meta.to_json().to_string())?;
        for old in self.reports().iter().skip(KEEP_REPORTS) {
            let _ = fs::remove_file(dir.join(format!("{}.md", old.id)));
            let _ = fs::remove_file(dir.join(format!("{}.json", old.id)));
        }
        Ok(id)
    }

    /// Newest first.
    pub fn reports(&self) -> Vec<ReportMeta> {
        let mut v = Self::load(&self.crashes(), "", ReportMeta::from_json);
        v.sort_by(|a, b| b.id.cmp(&a.id));
        v
    }

    pub fn report(&self, id: &str) -> Option<(ReportMeta, String)> {
        let meta = ReportMeta::from_json(&Self::read_json(
            &self.crashes().join(format!("{id}.json")),
        )?)?;
        Some((
            meta,
            fs::read_to_string(self.crashes().join(format!("{id}.md"))).ok()?,
        ))
    }

    pub fn path_of_report(&self, id: &str) -> PathBuf {
        self.crashes().join(format!("{id}.md"))
    }

    fn set_meta(&self, id: &str, f: impl FnOnce(&mut ReportMeta)) -> io::Result<()> {
        let (mut m, _) = self
            .report(id)
            .ok_or_else(|| io::Error::other("no such report"))?;
        f(&mut m);
        Self::write(
            &self.crashes().join(format!("{id}.json")),
            &m.to_json().to_string(),
        )
    }

    pub fn set_status(&self, id: &str, status: &str) -> io::Result<()> {
        self.set_meta(id, |m| m.status = status.into())
    }

    /// Adds the analysis section once.
    pub fn add_analysis(&self, id: &str, section: &str) -> io::Result<()> {
        let (m, body) = self
            .report(id)
            .ok_or_else(|| io::Error::other("no such report"))?;
        if m.analysed {
            return Ok(());
        }
        Self::write(&self.path_of_report(id), &(body + section))?;
        self.set_meta(id, |m| m.analysed = true)
    }

    /// Posts one report through `gh`: a comment on the open issue with the same
    /// signature, else a new issue. Marks it filed.
    pub fn send(&self, id: &str, gh: &dyn Gh, env: &Env) -> Result<Filed, String> {
        let (meta, body) = self.report(id).ok_or("the report is gone")?;
        let filed = crash::file(
            gh,
            &meta.signature,
            &meta.title,
            &body,
            &crash::occurrence_comment(env),
        )?;
        let url = match &filed {
            Filed::Opened(u) | Filed::Commented(u) => u.clone(),
        };
        let _ = self.set_meta(id, |m| (m.status, m.issue) = ("filed".into(), Some(url)));
        Ok(filed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crash::Kind;
    use std::sync::Mutex;

    fn temp(name: &str) -> Store {
        let d = std::env::temp_dir().join(format!("crabby-j-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        Store::at(d)
    }

    fn entry(id: &str, owner: u32, state: TaskState, started: u64) -> Entry {
        Entry {
            id: id.into(),
            owner,
            started,
            state,
            ..new_entry("run", "task text", "C:\\p")
        }
    }

    #[test]
    fn an_entry_survives_a_round_trip_through_json() {
        let mut e = entry("a", 7, TaskState::Crashed, 5);
        e.agent = Some("sc-x".into());
        e.session = Some("sid".into());
        e.replaces = Some("old".into());
        assert_eq!(Entry::from_json(&e.to_json()), Some(e));
        assert_eq!(Entry::from_json(&json!({"id": "a"})), None);
    }

    #[test]
    fn unfinished_entries_of_dead_windows_are_interrupted_and_a_live_windows_are_not() {
        let alive = |p: u32| p == 2;
        let all = [
            entry("dead-run", 1, TaskState::Running, 10),
            entry("live-run", 2, TaskState::Running, 11),
            entry("done", 1, TaskState::Finished, 12),
            entry("crashed", 1, TaskState::Crashed, 9),
            entry("mine", 99, TaskState::Running, 13),
            entry("old", 1, TaskState::Interrupted, 1),
            Entry {
                replaces: Some("old".into()),
                ..entry("new", 1, TaskState::Running, 14)
            },
        ];
        let ids: Vec<String> = interrupted(&all, 99, &alive)
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["crashed", "dead-run", "new"]);
    }

    #[test]
    fn only_the_newest_fifty_finished_entries_are_kept() {
        let mut all: Vec<Entry> = (0..60)
            .map(|i| entry(&format!("f{i}"), 1, TaskState::Finished, i))
            .collect();
        all.push(entry("running", 1, TaskState::Running, 0));
        let gone = prune_ids(&all, 50);
        assert_eq!(gone.len(), 10);
        assert!(gone.contains(&"f0".to_string()) && !gone.contains(&"f10".to_string()));
        assert!(!gone.contains(&"running".to_string()));
    }

    #[test]
    fn three_failed_start_ups_in_a_row_mean_safe_mode() {
        assert_eq!(plan(1, true, 1, 1), Plan::Offer);
        assert_eq!(plan(2, false, 0, 0), Plan::Nothing);
        assert_eq!(plan(0, false, 0, 1), Plan::Offer);
        assert_eq!(plan(3, true, 1, 1), Plan::Safe);
        let s = temp("loop");
        assert_eq!(
            (s.bump_startups(), s.bump_startups(), s.bump_startups()),
            (1, 2, 3)
        );
        s.clear_startups();
        assert_eq!(s.bump_startups(), 1);
        let _ = fs::remove_dir_all(s.base());
    }

    #[test]
    fn a_corrupt_or_half_written_file_is_skipped_and_removed() {
        let s = temp("corrupt");
        s.save_entry(&entry("good", 1, TaskState::Running, 1))
            .unwrap();
        let dir = s.base().join("journal");
        fs::write(dir.join("task-bad.json"), "{\"id\": \"bad\", \"own").unwrap();
        fs::write(dir.join("chat-bad.json"), "\u{0}\u{0}").unwrap();
        fs::write(dir.join("task-empty.json"), "").unwrap();
        let found = s.scan(99, &|_| false);
        assert_eq!(found.tasks.len(), 1);
        assert!(found.conversation.is_none());
        assert!(!dir.join("task-bad.json").exists() && !dir.join("chat-bad.json").exists());
        let _ = fs::remove_dir_all(s.base());
    }

    #[test]
    fn a_conversation_is_found_after_its_window_dies_and_not_before() {
        let s = temp("chat");
        let snap = Snapshot {
            id: "c1".into(),
            owner: 5,
            mode: "read".into(),
            session: Some("sid".into()),
            input: "unsent".into(),
            entries: vec![
                ("user".into(), "hi".into()),
                ("assistant".into(), "hello".into()),
            ],
            ..Default::default()
        };
        s.save_snapshot(&snap).unwrap();
        assert!(
            s.scan(99, &|p| p == 5).conversation.is_none(),
            "window 5 is still running"
        );
        assert_eq!(s.scan(99, &|_| false).conversation, Some(snap));
        s.forget_snapshot("c1");
        assert!(s.scan(99, &|_| false).conversation.is_none());
        let _ = fs::remove_dir_all(s.base());
    }

    #[test]
    fn forgetting_an_entry_also_drops_the_one_that_replaced_it_and_finishing_drops_the_old() {
        let s = temp("forget");
        s.save_entry(&entry("old", 1, TaskState::Interrupted, 1))
            .unwrap();
        let mut new = entry("new", 2, TaskState::Running, 2);
        new.replaces = Some("old".into());
        s.save_entry(&new).unwrap();
        s.update_entry("new", |e| e.state = TaskState::Finished)
            .unwrap();
        let ids: Vec<String> = s.entries().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["new"]);
        s.save_entry(&entry("old2", 1, TaskState::Interrupted, 1))
            .unwrap();
        s.save_entry(&Entry {
            replaces: Some("old2".into()),
            ..entry("new2", 2, TaskState::Crashed, 2)
        })
        .unwrap();
        s.forget("old2");
        assert_eq!(s.entries().len(), 1);
        let _ = fs::remove_dir_all(s.base());
    }

    #[test]
    fn consent_is_stored_and_defaults_to_ask() {
        let s = temp("consent");
        assert_eq!(s.consent(), Consent::Ask);
        s.set_consent(Consent::Never).unwrap();
        assert_eq!(s.consent(), Consent::Never);
        let _ = fs::remove_dir_all(s.base());
    }

    #[test]
    fn a_read_only_base_is_an_error_not_a_panic() {
        // A file where the folder should be: nothing can be created below it.
        let s = temp("ro");
        fs::create_dir_all(s.base()).unwrap();
        fs::write(s.base().join("journal"), "x").unwrap();
        assert!(s.save_entry(&entry("a", 1, TaskState::Running, 1)).is_err());
        assert!(s.entries().is_empty());
        assert_eq!(s.consent(), Consent::Ask);
        let _ = fs::remove_dir_all(s.base());
    }

    struct Recorder(Mutex<Vec<String>>);
    impl Gh for Recorder {
        fn open_issues(&self) -> Result<String, String> {
            Ok("[]".into())
        }
        fn create(&self, title: &str, body: &str) -> Result<String, String> {
            self.0.lock().unwrap().push(format!("{title}\n{body}"));
            Ok("https://example.test/1".into())
        }
        fn comment(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn a_saved_report_is_redacted_filed_once_and_listed_with_its_link() {
        let s = temp("report");
        let c = Crash {
            kind: Kind::Panic,
            location: "src/ui.rs:1:1".into(),
            message: "boom sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ".into(),
            detail: None,
            doing: None,
        };
        let id = s.save_report(&c, &Env::default()).unwrap();
        s.add_analysis(&id, "\n## Analysis\n\nguess\n").unwrap();
        s.add_analysis(&id, "\n## Analysis\n\nsecond\n").unwrap(); // only once
        let gh = Recorder(Mutex::new(vec![]));
        let filed = s.send(&id, &gh, &Env::default()).unwrap();
        assert_eq!(filed, Filed::Opened("https://example.test/1".into()));
        let posted = gh.0.lock().unwrap()[0].clone();
        assert!(posted.contains("guess") && !posted.contains("second"));
        assert!(!posted.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        let listed = list_text(&s.reports());
        assert!(listed.contains("https://example.test/1"), "{listed}");
        assert_eq!(s.report(&id).unwrap().0.status, "filed");
        let _ = fs::remove_dir_all(s.base());
    }

    #[test]
    fn old_reports_are_pruned_to_the_last_hundred() {
        let s = temp("prune");
        let c = Crash {
            kind: Kind::Task,
            location: "x".into(),
            message: "m".into(),
            detail: None,
            doing: None,
        };
        for _ in 0..(KEEP_REPORTS + 5) {
            s.save_report(&c, &Env::default()).unwrap();
        }
        assert_eq!(s.reports().len(), KEEP_REPORTS);
        let _ = fs::remove_dir_all(s.base());
    }
}
