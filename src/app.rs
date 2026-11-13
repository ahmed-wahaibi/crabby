//! The chat's state and its pure `update`. No terminal, no threads, no model calls:
//! `update` turns a message into changes to the state plus a list of [`Effect`]s that
//! `main` carries out.

use crate::{
    catalog::Agent,
    commands::{self, Command, Input},
    display,
    outcome::{self, Status},
    runner::Usage,
    settings::{self, Effort, Gate, Mode},
    ui,
};
use clap::ValueEnum;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Banner(String),
    User(String),
    Assistant(String),
    /// One tool call, shown dim on one line.
    Tool(String),
    Info(String),
    Error(String),
}

/// Work for a thread. Everything a model call needs is copied in, already gated.
#[derive(Debug)]
pub enum Job {
    Chat {
        prompt: String,
        session: Option<String>,
        mode: Mode,
        model: Option<String>,
        effort: Option<Effort>,
    },
    Run {
        agent: Agent,
        task: String,
        model: Option<String>,
        effort: Option<Effort>,
    },
    Plan {
        task: String,
    },
}

/// What a running job reports back.
#[derive(Debug)]
pub enum JobEvent {
    Text(String),
    Tool {
        name: String,
        arg: String,
    },
    Session(String),
    Done {
        output: String,
        usage: Option<Usage>,
    },
    Failed(String),
}

#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    Tick,
    Job { id: u64, event: JobEvent },
}

/// What `main` must do after an update.
#[derive(Debug)]
pub enum Effect {
    Start {
        id: u64,
        job: Job,
    },
    /// Kill the running job.
    Stop,
    Quit,
}

#[derive(Debug, PartialEq, Clone)]
enum Kind {
    Chat,
    Run(String),
    Plan,
}

#[derive(Debug)]
pub struct Busy {
    id: u64,
    kind: Kind,
    effort: Option<Effort>,
    /// Whether any text arrived, so a final message is not shown twice.
    streamed: bool,
}

/// A pending Fable question: `yes` keeps `effort`, anything else lowers it to medium.
#[derive(Debug)]
pub struct Confirm {
    effort: Effort,
}

#[derive(Debug, Default)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cost: f64,
}

pub struct State {
    pub entries: Vec<Entry>,
    pub input: String,
    /// Byte offset of the cursor in `input`.
    pub cursor: usize,
    history: Vec<String>,
    hist: Option<usize>,
    draft: String,
    /// Lines scrolled up from the bottom; 0 follows the newest.
    pub scroll: usize,
    pub busy: Option<Busy>,
    next_id: u64,
    session: Option<String>,
    pub model: Option<String>,
    /// What Claude Code uses without `--model`, if known.
    pub default_model: Option<String>,
    pub effort: Option<Effort>,
    pub mode: Mode,
    /// The user typed `yes` for the current model and effort.
    fable_ok: bool,
    pub confirm: Option<Confirm>,
    pub totals: Totals,
    pub cwd: String,
    pub color: bool,
    /// Draw the crab on the welcome screen: colour allowed and true colour reported.
    pub mascot: bool,
    pub width: u16,
    pub height: u16,
    pub tick: usize,
    pub armed: bool,
    agents: Arc<Vec<Agent>>,
}

impl State {
    pub fn new(
        agents: Arc<Vec<Agent>>,
        default_model: Option<String>,
        cwd: String,
        color: bool,
        size: (u16, u16),
        warnings: &[String],
    ) -> Self {
        let mut st = State {
            entries: vec![],
            input: String::new(),
            cursor: 0,
            history: vec![],
            hist: None,
            draft: String::new(),
            scroll: 0,
            busy: None,
            next_id: 1,
            session: None,
            model: None,
            default_model,
            effort: None,
            mode: Mode::Read,
            fable_ok: false,
            confirm: None,
            totals: Totals::default(),
            cwd,
            color,
            mascot: false,
            width: size.0,
            height: size.1,
            tick: 0,
            armed: false,
            agents,
        };
        st.banner(warnings);
        st
    }

    fn banner(&mut self, warnings: &[String]) {
        self.entries.push(Entry::Banner(format!(
            "crabby {} · SuperCrab agents through Claude Code",
            env!("CARGO_PKG_VERSION")
        )));
        self.entries.push(Entry::Info(
            "Type a message, or /help for the commands.".into(),
        ));
        for w in warnings {
            self.entries.push(Entry::Error(format!("skipped {w}")));
        }
    }

    /// The model the gate reasons about: the one set here, else Claude Code's own.
    fn known_model(&self) -> Option<&str> {
        self.model.as_deref().or(self.default_model.as_deref())
    }

    /// The effort a job gets: never Fable above medium without a `yes`.
    pub fn effective_effort(&self) -> Option<Effort> {
        settings::effective_effort(self.known_model(), self.effort, self.fable_ok)
    }

    fn push(&mut self, entry: Entry) {
        if self.scroll > 0 {
            // Keep what the user scrolled to in place while a reply grows below it.
            self.scroll += ui::entry_lines(&entry, self.width as usize, false).len();
        }
        self.entries.push(entry);
    }

    fn info(&mut self, s: impl Into<String>) {
        self.push(Entry::Info(s.into()));
    }

    fn error(&mut self, s: impl Into<String>) {
        self.push(Entry::Error(s.into()));
    }

    fn insert_str(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.hist = None;
    }

    fn set_input(&mut self, s: String) {
        self.cursor = s.len();
        self.input = s;
    }

    fn page(&self) -> usize {
        (self.height as usize / 2).max(1)
    }

    /// Words for the status line: what a reply in progress is.
    pub fn working(&self) -> Option<&'static str> {
        self.busy.as_ref().map(|b| match b.kind {
            Kind::Chat => "replying",
            Kind::Run(_) => "running",
            Kind::Plan => "planning",
        })
    }
}

pub fn update(st: &mut State, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Tick => st.tick = st.tick.wrapping_add(1),
        Msg::Resize(w, h) => (st.width, st.height) = (w, h),
        Msg::Paste(text) => {
            st.armed = false;
            st.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        }
        Msg::Key(k) => return key(st, k),
        Msg::Job { id, event } => job_event(st, id, event),
    }
    vec![]
}

fn key(st: &mut State, k: KeyEvent) -> Vec<Effect> {
    if k.kind == KeyEventKind::Release {
        return vec![];
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let newline = k
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
    if !(ctrl && k.code == KeyCode::Char('c')) {
        st.armed = false;
    }
    match k.code {
        KeyCode::Char('c') if ctrl => return interrupt(st, true),
        KeyCode::Esc => return interrupt(st, false),
        KeyCode::Enter if newline => st.insert_str("\n"),
        KeyCode::Enter => return submit(st),
        // AltGr on Windows arrives as Ctrl+Alt; everything else with Ctrl is a shortcut.
        KeyCode::Char(c) if !ctrl || k.modifiers.contains(KeyModifiers::ALT) => {
            st.insert_str(c.encode_utf8(&mut [0; 4]));
        }
        KeyCode::Backspace => {
            if let Some(c) = st.input[..st.cursor].chars().next_back() {
                st.cursor -= c.len_utf8();
                st.input.remove(st.cursor);
            }
        }
        KeyCode::Delete => {
            if st.cursor < st.input.len() {
                st.input.remove(st.cursor);
            }
        }
        KeyCode::Left => {
            if let Some(c) = st.input[..st.cursor].chars().next_back() {
                st.cursor -= c.len_utf8();
            }
        }
        KeyCode::Right => {
            if let Some(c) = st.input[st.cursor..].chars().next() {
                st.cursor += c.len_utf8();
            }
        }
        KeyCode::Home => st.cursor = st.input[..st.cursor].rfind('\n').map_or(0, |i| i + 1),
        KeyCode::End => {
            st.cursor = st.input[st.cursor..]
                .find('\n')
                .map_or(st.input.len(), |i| st.cursor + i);
        }
        KeyCode::Up => recall(st, true),
        KeyCode::Down => recall(st, false),
        KeyCode::Tab => {
            if let Some(done) = commands::complete(&st.input) {
                st.set_input(done);
            }
        }
        KeyCode::PageUp => {
            let top = ui::total_lines(st).saturating_sub(1);
            st.scroll = (st.scroll + st.page()).min(top);
        }
        KeyCode::PageDown => st.scroll = st.scroll.saturating_sub(st.page()),
        _ => {}
    }
    vec![]
}

/// Up and Down walk the inputs sent earlier; Down past the newest restores the draft.
fn recall(st: &mut State, up: bool) {
    let n = st.history.len();
    let next = match (st.hist, up) {
        (None, true) if n > 0 => {
            st.draft = std::mem::take(&mut st.input);
            Some(n - 1)
        }
        (Some(i), true) => Some(i.saturating_sub(1)),
        (Some(i), false) if i + 1 < n => Some(i + 1),
        (Some(_), false) => None,
        _ => return,
    };
    st.hist = next;
    let text = match next {
        Some(i) => st.history[i].clone(),
        None => std::mem::take(&mut st.draft),
    };
    st.set_input(text);
}

/// Esc and Ctrl+C: answer a pending question with no, stop a reply, or (Ctrl+C on an
/// idle prompt, twice) quit.
fn interrupt(st: &mut State, ctrl_c: bool) -> Vec<Effect> {
    if st.confirm.is_some() {
        answer_confirm(st, false);
    } else if st.busy.take().is_some() {
        st.info("stopped");
        return vec![Effect::Stop];
    } else if ctrl_c {
        if st.armed {
            return vec![Effect::Quit];
        }
        st.armed = true;
        st.input.clear();
        st.cursor = 0;
    }
    vec![]
}

fn submit(st: &mut State) -> Vec<Effect> {
    let text = st.input.trim().to_string();
    if st.confirm.is_some() {
        (st.input, st.cursor) = (String::new(), 0);
        st.push(Entry::User(text.clone()));
        answer_confirm(st, text == "yes");
        return vec![];
    }
    if text.is_empty() {
        return vec![];
    }
    let parsed = commands::parse(&text);
    let starts_job = matches!(
        parsed,
        Ok(Input::Message(_) | Input::Command(Command::Run { .. } | Command::Plan(_)))
    );
    if st.busy.is_some() && starts_job {
        st.info("a reply is still running; Esc stops it, or wait for it to finish");
        return vec![];
    }
    if st.history.last() != Some(&text) {
        st.history.push(text.clone());
    }
    (st.input, st.cursor, st.hist, st.scroll) = (String::new(), 0, None, 0);
    st.push(Entry::User(text));
    match parsed {
        Err(e) => {
            st.error(e);
            vec![]
        }
        Ok(Input::Message(prompt)) => {
            let job = Job::Chat {
                prompt,
                session: st.session.clone(),
                mode: st.mode,
                model: st.model.clone(),
                effort: st.effective_effort(),
            };
            begin(st, Kind::Chat, job)
        }
        Ok(Input::Command(c)) => command(st, c),
    }
}

fn begin(st: &mut State, kind: Kind, job: Job) -> Vec<Effect> {
    let id = st.next_id;
    st.next_id += 1;
    let effort = match &job {
        Job::Chat { effort, .. } | Job::Run { effort, .. } => *effort,
        Job::Plan { .. } => None,
    };
    st.busy = Some(Busy {
        id,
        kind,
        effort,
        streamed: false,
    });
    vec![Effect::Start { id, job }]
}

fn command(st: &mut State, c: Command) -> Vec<Effect> {
    match c {
        Command::Help => st.info(commands::help()),
        Command::Agents(text) => {
            let agents = st.agents.clone();
            let shown = commands::filter(&agents, &text);
            if shown.is_empty() {
                st.info(format!("no agent matches `{text}`"));
            } else {
                let list = display::render_list(&shown, Some(st.width as usize));
                st.info(list.trim_end());
            }
        }
        Command::Run { agent, task } => {
            let agents = st.agents.clone();
            match commands::find_agent(&agents, &agent) {
                Err(e) => st.error(e),
                Ok(a) => {
                    let job = Job::Run {
                        agent: a.clone(),
                        task,
                        model: st.model.clone(),
                        effort: st.effective_effort(),
                    };
                    return begin(st, Kind::Run(a.name.clone()), job);
                }
            }
        }
        Command::Plan(task) => return begin(st, Kind::Plan, Job::Plan { task }),
        Command::Model(None) => {
            let shown = match (&st.model, &st.default_model) {
                (Some(m), _) => m.clone(),
                (None, Some(d)) => format!("default ({d})"),
                (None, None) => "default (not known)".into(),
            };
            st.info(format!("model: {shown}"));
        }
        Command::Model(Some(m)) => {
            let m = m.to_ascii_lowercase();
            if m == "default" {
                st.model = None;
            } else if let Err(e) = settings::check_model(&m) {
                st.error(e);
                return vec![];
            } else {
                st.model = Some(m);
            }
            st.fable_ok = false;
            st.info(format!(
                "model set to {}",
                st.model.as_deref().unwrap_or("default")
            ));
            if let Some(e) = st.effort {
                ask_fable(st, e);
            }
        }
        Command::Effort(None) => {
            let shown = st.effort.map_or("default".into(), Effort::name);
            st.info(format!("effort: {shown}"));
        }
        Command::Effort(Some(v)) => {
            if v.eq_ignore_ascii_case("default") {
                (st.effort, st.fable_ok) = (None, false);
                st.info("effort set to default");
            } else if let Ok(e) = Effort::from_str(&v, true) {
                st.fable_ok = false;
                if !ask_fable(st, e) {
                    st.effort = Some(e);
                    st.info(format!("effort set to {}", e.name()));
                }
            } else {
                st.error(format!(
                    "unknown effort `{v}`; use low, medium, high, xhigh, max or default"
                ));
            }
        }
        Command::Mode(None) => st.info(format!("mode: {}", st.mode.name())),
        Command::Mode(Some(v)) => match Mode::from_str(&v, true) {
            Ok(m) => {
                st.mode = m;
                st.info(format!("mode set to {}", m.name()));
            }
            Err(_) => st.error(format!("unknown mode `{v}`; use read or edit")),
        },
        Command::Clear => {
            let stop = st.busy.take().is_some();
            st.session = None;
            st.entries.clear();
            st.scroll = 0;
            st.banner(&[]);
            return if stop { vec![Effect::Stop] } else { vec![] };
        }
        Command::Quit => {
            return if st.busy.take().is_some() {
                vec![Effect::Stop, Effect::Quit]
            } else {
                vec![Effect::Quit]
            };
        }
    }
    vec![]
}

/// Starts the Fable question when `effort` would need it. True when it did, in which
/// case the effort stays as it was until the answer.
fn ask_fable(st: &mut State, effort: Effort) -> bool {
    if settings::gate(st.known_model(), Some(effort), true) != Gate::Ask || st.fable_ok {
        return false;
    }
    let which = st.known_model().map_or(
        "a model that could not be determined (it may be Fable)".to_string(),
        |m| format!("model {m}"),
    );
    st.info(format!(
        "{which} above medium effort costs more. Type `yes` to use {}; anything else uses medium.",
        effort.name()
    ));
    st.confirm = Some(Confirm { effort });
    true
}

fn answer_confirm(st: &mut State, yes: bool) {
    let Some(Confirm { effort }) = st.confirm.take() else {
        return;
    };
    if yes {
        (st.effort, st.fable_ok) = (Some(effort), true);
        st.info(format!("effort set to {}", effort.name()));
    } else {
        (st.effort, st.fable_ok) = (Some(Effort::Medium), false);
        st.info("effort set to medium");
    }
}

fn job_event(st: &mut State, id: u64, event: JobEvent) {
    let Some(busy) = st.busy.as_mut().filter(|b| b.id == id) else {
        return; // from a job that was stopped
    };
    match event {
        JobEvent::Text(t) => {
            busy.streamed = true;
            st.push(Entry::Assistant(t));
        }
        JobEvent::Tool { name, arg } => {
            let arg: String = arg.lines().next().unwrap_or("").chars().take(100).collect();
            st.push(Entry::Tool(format!("{name} {arg}").trim_end().to_string()));
        }
        JobEvent::Session(s) => {
            if busy.kind == Kind::Chat {
                st.session = Some(s);
            }
        }
        JobEvent::Failed(e) => {
            st.busy = None;
            st.error(e);
        }
        JobEvent::Done { output, usage } => {
            let Busy {
                kind,
                effort,
                streamed,
                ..
            } = st.busy.take().expect("checked above");
            if !streamed && !output.trim().is_empty() {
                st.push(Entry::Assistant(output.clone()));
            }
            if let Kind::Run(name) = &kind {
                let status = match outcome::parse(&output).and_then(|o| o.status) {
                    Some(Status::Done) => "done",
                    Some(Status::Partial) => "partial",
                    Some(Status::Blocked) => "blocked",
                    None => "finished without a result block, so its status is unknown",
                };
                st.info(format!("{name}: {status}"));
            }
            if let Some(u) = usage {
                st.totals.input += u.input_tokens;
                st.totals.output += u.output_tokens;
                st.totals.cost += u.cost_usd.unwrap_or(0.0);
                if kind != Kind::Chat {
                    st.info(display::summary(&u, effort));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn agent(name: &str) -> Agent {
        Agent {
            name: name.into(),
            description: "Does things.".into(),
            category: "cat".into(),
            tools: vec!["Read".into(), "Grep".into()],
            body: "body".into(),
        }
    }

    fn state() -> State {
        let agents = Arc::new(vec![agent("sc-rust-engineer"), agent("sc-debugger")]);
        State::new(
            agents,
            Some("opus".into()),
            "C:\\proj".into(),
            true,
            (80, 24),
            &[],
        )
    }

    fn press(st: &mut State, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
        update(
            st,
            Msg::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        )
    }

    fn type_text(st: &mut State, s: &str) {
        for c in s.chars() {
            press(st, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    fn enter(st: &mut State, s: &str) -> Vec<Effect> {
        type_text(st, s);
        press(st, KeyCode::Enter, KeyModifiers::NONE)
    }

    fn job(st: &mut State, event: JobEvent) {
        let id = st.busy.as_ref().expect("a job is running").id;
        update(st, Msg::Job { id, event });
    }

    fn last(st: &State) -> &Entry {
        st.entries.last().unwrap()
    }

    fn started(effects: Vec<Effect>) -> Job {
        match effects.into_iter().next() {
            Some(Effect::Start { job, .. }) => job,
            e => panic!("expected a job, got {e:?}"),
        }
    }

    fn done(output: &str) -> JobEvent {
        JobEvent::Done {
            output: output.into(),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cost_usd: Some(0.5),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn typing_and_editing_the_input() {
        let mut st = state();
        type_text(&mut st, "héllo");
        press(&mut st, KeyCode::Left, KeyModifiers::NONE);
        press(&mut st, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.input, "hélo");
        press(&mut st, KeyCode::Home, KeyModifiers::NONE);
        press(&mut st, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(st.input, "élo");
        press(&mut st, KeyCode::End, KeyModifiers::NONE);
        press(&mut st, KeyCode::Enter, KeyModifiers::ALT);
        type_text(&mut st, "2");
        assert_eq!(st.input, "élo\n2");
        update(&mut st, Msg::Paste("a\r\nb".into()));
        assert_eq!(st.input, "élo\n2a\nb");
        // Key releases (sent on Windows) change nothing.
        let release = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        update(&mut st, Msg::Key(release));
        assert_eq!(st.input, "élo\n2a\nb");
    }

    #[test]
    fn a_message_starts_a_chat_and_the_follow_up_resumes_it() {
        let mut st = state();
        let Job::Chat {
            prompt, session, ..
        } = started(enter(&mut st, "hello"))
        else {
            panic!("chat")
        };
        assert_eq!((prompt.as_str(), session), ("hello", None));
        assert_eq!(st.input, "");
        assert_eq!(st.working(), Some("replying"));
        job(&mut st, JobEvent::Session("sid-1".into()));
        job(
            &mut st,
            JobEvent::Tool {
                name: "Read".into(),
                arg: "a.rs\nmore".into(),
            },
        );
        job(&mut st, JobEvent::Text("hi".into()));
        job(&mut st, done("hi"));
        assert!(st.busy.is_none());
        assert_eq!(
            st.entries[st.entries.len() - 2],
            Entry::Tool("Read a.rs".into())
        );
        // The final text was already streamed, so it is not shown twice.
        assert_eq!(
            st.entries
                .iter()
                .filter(|e| **e == Entry::Assistant("hi".into()))
                .count(),
            1
        );
        assert_eq!((st.totals.input, st.totals.output), (10, 5));

        let Job::Chat { session, .. } = started(enter(&mut st, "and then?")) else {
            panic!("chat")
        };
        assert_eq!(session.as_deref(), Some("sid-1"));
    }

    #[test]
    fn a_reply_that_streamed_nothing_is_shown_from_the_result() {
        let mut st = state();
        enter(&mut st, "hello");
        job(&mut st, done("only in the result"));
        assert!(st
            .entries
            .contains(&Entry::Assistant("only in the result".into())));
    }

    #[test]
    fn a_message_while_a_reply_streams_is_refused_and_kept() {
        let mut st = state();
        enter(&mut st, "first");
        assert!(enter(&mut st, "second").is_empty());
        assert_eq!(st.input, "second");
        assert!(matches!(last(&st), Entry::Info(m) if m.contains("still running")));
        // Local commands still work.
        assert!(press(&mut st, KeyCode::Char('a'), KeyModifiers::NONE).is_empty());
        (st.input, st.cursor) = (String::new(), 0);
        enter(&mut st, "/mode edit");
        assert_eq!(st.mode, Mode::Edit);
    }

    #[test]
    fn escape_stops_the_reply_and_late_events_are_ignored() {
        let mut st = state();
        enter(&mut st, "go");
        let id = st.busy.as_ref().unwrap().id;
        let fx = press(&mut st, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(fx.as_slice(), [Effect::Stop]));
        assert!(st.busy.is_none());
        let n = st.entries.len();
        update(
            &mut st,
            Msg::Job {
                id,
                event: JobEvent::Text("late".into()),
            },
        );
        update(
            &mut st,
            Msg::Job {
                id,
                event: JobEvent::Failed("stopped".into()),
            },
        );
        assert_eq!(st.entries.len(), n);
        // The interface is usable: the next message starts a new job.
        assert!(matches!(started(enter(&mut st, "again")), Job::Chat { .. }));
        // Ctrl+C stops a reply too, and does not arm the quit.
        let fx = press(&mut st, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(fx.as_slice(), [Effect::Stop]));
        assert!(!st.armed);
    }

    #[test]
    fn ctrl_c_twice_on_an_idle_prompt_quits() {
        let mut st = state();
        type_text(&mut st, "draft");
        assert!(press(&mut st, KeyCode::Char('c'), KeyModifiers::CONTROL).is_empty());
        assert!(st.armed);
        assert_eq!(st.input, "");
        let fx = press(&mut st, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(fx.as_slice(), [Effect::Quit]));
        // Any other key disarms.
        let mut st = state();
        press(&mut st, KeyCode::Char('c'), KeyModifiers::CONTROL);
        type_text(&mut st, "x");
        assert!(press(&mut st, KeyCode::Char('c'), KeyModifiers::CONTROL).is_empty());
    }

    #[test]
    fn a_failed_job_shows_the_error_and_frees_the_prompt() {
        let mut st = state();
        enter(&mut st, "go");
        job(&mut st, JobEvent::Failed("`claude` was not found".into()));
        assert!(st.busy.is_none());
        assert_eq!(*last(&st), Entry::Error("`claude` was not found".into()));
    }

    #[test]
    fn history_recall_walks_back_and_restores_the_draft() {
        let mut st = state();
        enter(&mut st, "/mode edit");
        enter(&mut st, "/mode read");
        type_text(&mut st, "dra");
        press(&mut st, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(st.input, "/mode read");
        press(&mut st, KeyCode::Up, KeyModifiers::NONE);
        press(&mut st, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(st.input, "/mode edit");
        press(&mut st, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(st.input, "/mode read");
        press(&mut st, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(st.input, "dra");
    }

    #[test]
    fn tab_completes_commands() {
        let mut st = state();
        type_text(&mut st, "/ru");
        press(&mut st, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(st.input, "/run ");
        assert_eq!(st.cursor, 5);
    }

    #[test]
    fn help_agents_and_unknown_commands() {
        let mut st = state();
        enter(&mut st, "/help");
        assert!(matches!(last(&st), Entry::Info(m) if m.contains("/plan <task>")));
        enter(&mut st, "/agents debug");
        assert!(
            matches!(last(&st), Entry::Info(m) if m.contains("sc-debugger") && !m.contains("rust"))
        );
        enter(&mut st, "/agents zzz");
        assert!(matches!(last(&st), Entry::Info(m) if m.contains("no agent matches")));
        enter(&mut st, "/nope");
        assert!(matches!(last(&st), Entry::Error(m) if m.contains("unknown command")));
        enter(&mut st, "/run sc-debugger");
        assert!(matches!(last(&st), Entry::Error(m) if m.starts_with("usage:")));
        assert!(st.busy.is_none());
    }

    #[test]
    fn run_uses_the_agent_and_ends_with_status_and_usage() {
        let mut st = state();
        let Job::Run { agent, task, .. } = started(enter(&mut st, "/run sc-debugger find the bug"))
        else {
            panic!("run")
        };
        assert_eq!(
            (agent.name.as_str(), task.as_str()),
            ("sc-debugger", "find the bug")
        );
        assert_eq!(agent.tools, ["Read", "Grep"]);
        job(
            &mut st,
            done("all fine\n## Result\n- status: partial\n- summary: x"),
        );
        let tail: Vec<&Entry> = st.entries.iter().rev().take(2).collect();
        assert!(
            matches!(tail[0], Entry::Info(m) if m.starts_with("crabby:")),
            "{tail:?}"
        );
        assert_eq!(*tail[1], Entry::Info("sc-debugger: partial".into()));

        enter(&mut st, "/run sc-debugger t");
        job(&mut st, done("no block"));
        assert!(st
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Info(m) if m.contains("status is unknown"))));
    }

    #[test]
    fn run_with_an_unknown_agent_suggests_one() {
        let mut st = state();
        assert!(enter(&mut st, "/run debugger x").is_empty() || st.busy.is_none());
        assert!(matches!(last(&st), Entry::Error(m) if m.contains("did you mean: sc-debugger")));
    }

    #[test]
    fn plan_runs_the_router_job() {
        let mut st = state();
        assert!(
            matches!(started(enter(&mut st, "/plan add tests")), Job::Plan { task } if task == "add tests")
        );
        assert_eq!(st.working(), Some("planning"));
        job(
            &mut st,
            JobEvent::Text("1  sc-a  sonnet · low  do it".into()),
        );
        job(&mut st, done("1  sc-a  sonnet · low  do it"));
        assert!(matches!(last(&st), Entry::Info(m) if m.starts_with("crabby:")));
    }

    #[test]
    fn model_effort_and_mode_show_and_set() {
        let mut st = state();
        enter(&mut st, "/model");
        assert_eq!(*last(&st), Entry::Info("model: default (opus)".into()));
        enter(&mut st, "/model Sonnet");
        assert_eq!(st.model.as_deref(), Some("sonnet"));
        enter(&mut st, "/model gpt-4");
        assert!(matches!(last(&st), Entry::Error(_)));
        assert_eq!(st.model.as_deref(), Some("sonnet"));
        enter(&mut st, "/model default");
        assert_eq!(st.model, None);

        enter(&mut st, "/effort high");
        assert_eq!(st.effort, Some(Effort::High));
        enter(&mut st, "/effort loud");
        assert!(matches!(last(&st), Entry::Error(m) if m.contains("unknown effort")));
        enter(&mut st, "/effort");
        assert_eq!(*last(&st), Entry::Info("effort: high".into()));

        enter(&mut st, "/mode");
        assert_eq!(*last(&st), Entry::Info("mode: read".into()));
        enter(&mut st, "/mode edit");
        assert_eq!(st.mode, Mode::Edit);
        enter(&mut st, "/mode wild");
        assert!(matches!(last(&st), Entry::Error(_)));
        assert_eq!(st.mode, Mode::Edit);
        // Settings reach the next job.
        let Job::Chat {
            mode,
            effort,
            model,
            ..
        } = started(enter(&mut st, "hi"))
        else {
            panic!("chat")
        };
        assert_eq!(
            (mode, effort, model),
            (Mode::Edit, Some(Effort::High), None)
        );
    }

    #[test]
    fn fable_above_medium_needs_a_typed_yes() {
        let mut st = state();
        enter(&mut st, "/model fable");
        assert!(st.confirm.is_none()); // no effort set yet
        enter(&mut st, "/effort max");
        assert!(st.confirm.is_some());
        assert_eq!(st.effort, None); // unchanged until answered
        assert!(matches!(last(&st), Entry::Info(m) if m.contains("Type `yes`")));
        // Even before the answer, a job cannot run above medium.
        assert_eq!(st.effective_effort(), Some(Effort::Medium));
        // The answer is consumed, not sent as a message.
        assert!(enter(&mut st, "yes").is_empty());
        assert_eq!(st.effort, Some(Effort::Max));
        assert_eq!(st.effective_effort(), Some(Effort::Max));
        let Job::Chat { effort, .. } = started(enter(&mut st, "go")) else {
            panic!("chat")
        };
        assert_eq!(effort, Some(Effort::Max));
        job(&mut st, done("x"));
        // Changing the effort again asks again.
        enter(&mut st, "/effort high");
        assert!(st.confirm.is_some());
        assert_eq!(st.effective_effort(), Some(Effort::Medium));
    }

    #[test]
    fn anything_but_yes_lowers_fable_to_medium() {
        for no in ["no", "Yes", "yes please", ""] {
            let mut st = state();
            enter(&mut st, "/model fable");
            enter(&mut st, "/effort high");
            if no.is_empty() {
                press(&mut st, KeyCode::Esc, KeyModifiers::NONE);
            } else {
                enter(&mut st, no);
            }
            assert_eq!(st.effort, Some(Effort::Medium), "{no:?}");
            assert!(st.confirm.is_none());
            assert_eq!(st.effective_effort(), Some(Effort::Medium));
        }
    }

    #[test]
    fn switching_to_fable_with_a_high_effort_asks() {
        let mut st = state();
        enter(&mut st, "/effort max"); // default model is opus: fine
        assert_eq!(st.effort, Some(Effort::Max));
        assert!(st.confirm.is_none());
        enter(&mut st, "/model fable");
        assert!(st.confirm.is_some());
        assert_eq!(st.effective_effort(), Some(Effort::Medium));
        enter(&mut st, "no");
        assert_eq!(st.effort, Some(Effort::Medium));
    }

    #[test]
    fn an_unknown_model_is_treated_as_possibly_fable() {
        let agents = Arc::new(vec![agent("sc-a")]);
        let mut st = State::new(agents, None, "c".into(), false, (80, 24), &[]);
        enter(&mut st, "/effort high");
        assert!(st.confirm.is_some());
        let mut st = State::new(Arc::new(vec![]), None, "c".into(), false, (80, 24), &[]);
        // No effort set: jobs still get `medium`, not Claude Code's default.
        assert_eq!(st.effective_effort(), Some(Effort::Medium));
        enter(&mut st, "/model opus");
        assert_eq!(st.effective_effort(), None);
    }

    #[test]
    fn clear_starts_a_new_conversation() {
        let mut st = state();
        enter(&mut st, "hello");
        job(&mut st, JobEvent::Session("sid-1".into()));
        job(&mut st, done("x"));
        enter(&mut st, "/clear");
        assert!(st.entries.iter().all(|e| !matches!(e, Entry::User(_))));
        let Job::Chat { session, .. } = started(enter(&mut st, "fresh")) else {
            panic!("chat")
        };
        assert_eq!(session, None);
        // Clearing while a reply streams stops it.
        let fx = enter(&mut st, "/clear");
        assert!(matches!(fx.as_slice(), [Effect::Stop]));
        assert!(st.busy.is_none());
    }

    #[test]
    fn quit_exits_and_stops_a_running_job() {
        let mut st = state();
        assert!(matches!(enter(&mut st, "/quit").as_slice(), [Effect::Quit]));
        let mut st = state();
        enter(&mut st, "hi");
        assert!(matches!(
            enter(&mut st, "/quit").as_slice(),
            [Effect::Stop, Effect::Quit]
        ));
    }

    #[test]
    fn session_ids_from_agent_runs_are_not_adopted() {
        let mut st = state();
        enter(&mut st, "/run sc-debugger x");
        job(&mut st, JobEvent::Session("run-session".into()));
        job(&mut st, done("x"));
        let Job::Chat { session, .. } = started(enter(&mut st, "hello")) else {
            panic!("chat")
        };
        assert_eq!(session, None);
    }

    #[test]
    fn scrolling_clamps_and_holds_its_place_while_text_arrives() {
        let mut st = state();
        update(&mut st, Msg::Resize(40, 12));
        enter(&mut st, "hello");
        for i in 0..30 {
            job(&mut st, JobEvent::Text(format!("line {i}")));
        }
        press(&mut st, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(st.scroll, 6);
        job(&mut st, JobEvent::Text("new".into()));
        assert_eq!(st.scroll, 7);
        for _ in 0..20 {
            press(&mut st, KeyCode::PageUp, KeyModifiers::NONE);
        }
        assert!(st.scroll < ui::total_lines(&st));
        for _ in 0..20 {
            press(&mut st, KeyCode::PageDown, KeyModifiers::NONE);
        }
        assert_eq!(st.scroll, 0);
        // Sending jumps back to the newest.
        press(&mut st, KeyCode::PageUp, KeyModifiers::NONE);
        job(&mut st, done("x"));
        enter(&mut st, "again");
        assert_eq!(st.scroll, 0);
    }

    #[test]
    fn startup_warnings_are_shown() {
        let st = State::new(
            Arc::new(vec![]),
            None,
            "c".into(),
            false,
            (80, 24),
            &["a.md: bad".into()],
        );
        assert!(st
            .entries
            .contains(&Entry::Error("skipped a.md: bad".into())));
    }
}
