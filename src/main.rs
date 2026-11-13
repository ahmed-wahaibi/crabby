mod app;
mod catalog;
mod commands;
mod crash;
mod display;
mod journal;
mod mascot;
mod outcome;
mod router;
mod runner;
mod settings;
mod ui;

use app::{Effect, Job, JobEvent, Msg, State};
use catalog::Agent;
use clap::Parser;
use commands::{Command, Input};
use crash::{Consent, Crash, Doing, Env, Failure, Filed, Gh};
use journal::{Found, Store, TaskState};
use outcome::Status;
use ratatui::crossterm::{
    cursor::Show,
    event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event},
    execute,
};
use settings::{Effort, Mode};
use std::{
    io::{self, IsTerminal, Read, Write},
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

const AFTER_HELP: &str = "\
Examples:
  crabby                                   open the chat
  crabby -p \"what does src/main.rs do?\"    one message, reply on stdout
  crabby -p \"/plan add rate limiting to the upload endpoint\"
  crabby -p \"/run sc-rust-engineer make the parser report line numbers\"
  crabby -p - < task.txt                   read the input from stdin
  crabby -p \"/agents rust\" | cut -f1

In the chat, type /help for the commands: /agents, /run, /plan, /model, /effort,
/mode, /crashes, /crash-reports, /clear and /quit. A plain message is a conversation with one Claude Code
session in the current folder, in `read` mode (read and search tools only) unless
you set `/mode edit` or `--mode edit`.

Effort ceiling: a run on Fable (`fable` or `claude-fable-*`) is capped at `medium`,
and without an effort it is passed `--effort medium`. Asking for more asks you to
type `yes` in the chat; with -p there is nobody to ask, so it runs at `medium`.
Without `--model` crabby reads the model from ANTHROPIC_MODEL or Claude Code's
settings files, and if it cannot tell, it treats the run as possibly Fable.

Exit codes with -p:
   0  done (a reply, a plan or a list; for /run, the agent reported `done`)
   1  /run: the agent reported `partial`
   2  /run: the agent reported `blocked`
   3  /run: the agent finished without a result block
   4  crabby could not do it (unknown agent, no agents found, `claude` missing or failed,
      or a crash: a report is written and nothing is asked)
  64  usage error (bad flag, bad command, or no terminal and no -p)";

#[derive(Parser)]
#[command(name = "crabby", version, about = "Interactive chat for Claude Code with the SuperCrab agents", after_help = AFTER_HELP)]
struct Cli {
    /// Run one input without the interface and print the result: a message or a
    /// /command. Use `-` to read it from stdin
    #[arg(short = 'p', long = "print", value_name = "INPUT")]
    print: Option<String>,
    /// Read agents from this `categories` folder instead of the built-in set
    #[arg(long, value_name = "DIR")]
    agents_dir: Option<PathBuf>,
    /// Model for messages and /run (defaults to your Claude Code setting)
    #[arg(long)]
    model: Option<String>,
    /// Reasoning effort. On Fable, above `medium` needs a typed `yes` (see --help)
    #[arg(long, value_enum)]
    effort: Option<Effort>,
    /// What a plain conversation may do: `read` pre-approves read and search tools,
    /// `edit` also file edits and shell commands
    #[arg(long, value_enum, default_value = "read")]
    mode: Mode,
}

/// Exit code for anything crabby itself could not do.
const FAILED: u8 = 4;
const USAGE: u8 = 64;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(FAILED)
}

fn usage(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {msg}\nFor more information, try '--help'.");
    ExitCode::from(USAGE)
}

/// The exit code for the status in an agent's result block.
fn status_exit(status: Option<Status>) -> u8 {
    match status {
        Some(Status::Done) => 0,
        Some(Status::Partial) => 1,
        Some(Status::Blocked) => 2,
        None => 3,
    }
}

/// Where crash reports and the journal live; set once at start-up.
static STORE: OnceLock<Store> = OnceLock::new();
/// Whether the full-screen interface is up, so a panic knows to restore the terminal.
static IN_CHAT: AtomicBool = AtomicBool::new(false);

fn store() -> Option<&'static Store> {
    STORE.get()
}

/// Panics on purpose when `CRABBY_TEST_PANIC` names this place (`print`, `loop`, `draw`
/// or `job`), so the crash path can be tried without a bug. Not documented in --help.
fn deliberate(place: &str) {
    if std::env::var("CRABBY_TEST_PANIC").is_ok_and(|v| v == place) {
        panic!("deliberate test panic ({place})");
    }
}

/// What a job is, for the report. Never the task text.
fn doing_of(job: &Job) -> Doing {
    let effort = |e: &Option<Effort>| e.map(Effort::name);
    match job {
        Job::Chat {
            model, effort: e, ..
        } => Doing {
            task: "message".into(),
            agent: None,
            model: model.clone(),
            effort: effort(e),
        },
        Job::Run {
            agent,
            model,
            effort: e,
            ..
        } => Doing {
            task: "run".into(),
            agent: Some(agent.name.clone()),
            model: model.clone(),
            effort: effort(e),
        },
        Job::Plan { .. } => Doing {
            task: "plan".into(),
            agent: None,
            model: Some("haiku".into()),
            effort: Some("low".into()),
        },
    }
}

/// The journal entry for a job about to start, in `folder`.
fn entry_of(job: &Job, folder: &str, replaces: Option<&journal::Entry>) -> journal::Entry {
    let mut e = match job {
        Job::Chat {
            prompt,
            session,
            mode,
            model,
            effort,
        } => {
            let mut e = journal::new_entry("chat", prompt, folder);
            e.mode = Some(mode.name().into());
            e.model = model.clone();
            e.effort = effort.map(Effort::name);
            e.base_session = session.clone();
            e.session = session.clone().filter(|_| prompt == app::CONTINUE);
            e
        }
        Job::Run {
            agent,
            task,
            model,
            effort,
            resume,
        } => {
            let mut e = journal::new_entry("run", task, folder);
            e.agent = Some(agent.name.clone());
            e.model = model.clone();
            e.effort = effort.map(Effort::name);
            e.session = resume.clone();
            e
        }
        Job::Plan { task } => journal::new_entry("plan", task, folder),
    };
    if let Some(old) = replaces {
        // What the user originally asked, not the "continue" prompt.
        (e.input, e.base_session, e.replaces) = (
            old.input.clone(),
            old.base_session.clone(),
            Some(old.id.clone()),
        );
    }
    e
}

/// Runs a job to its end on the calling thread, reporting through `emit`. The last
/// event is always `Done`, `Failed` or `Crashed`.
fn execute(job: Job, agents: &[Agent], cancel: &AtomicBool, mut emit: impl FnMut(JobEvent)) {
    deliberate("job");
    let mut forward = |e: runner::Event| {
        emit(match e {
            runner::Event::Text(t) => JobEvent::Text(t),
            runner::Event::Tool { name, arg } => JobEvent::Tool { name, arg },
            runner::Event::Session(s) => JobEvent::Session(s),
        })
    };
    let result = match job {
        Job::Chat {
            prompt,
            session,
            mode,
            model,
            effort,
        } => runner::chat(
            &prompt,
            mode.tools(),
            session.as_deref(),
            model.as_deref(),
            effort.map(Effort::name).as_deref(),
            cancel,
            &mut forward,
        ),
        Job::Run {
            agent,
            task,
            model,
            effort,
            resume,
        } => runner::run(
            &agent,
            &task,
            model.as_deref(),
            effort.map(Effort::name).as_deref(),
            resume.as_deref(),
            cancel,
            &mut forward,
        ),
        // The router is always Haiku at low effort, which the Fable ceiling never touches.
        Job::Plan { task } => {
            router::plan(agents, &task, "haiku", "low", cancel).map(|(plan, usage)| {
                let text = router::render(&plan);
                forward(runner::Event::Text(text.trim_end().to_string()));
                (text, Some(usage))
            })
        }
    };
    emit(match result {
        Ok((output, usage)) => JobEvent::Done { output, usage },
        Err(e) => match crash::classify(e.kind(), &e.to_string()) {
            Failure::Crash => JobEvent::Crashed {
                crash: crash::from_failure(&e.to_string(), crash::current_doing()),
                report: None,
            },
            Failure::Stopped | Failure::Problem => JobEvent::Failed(e.to_string()),
        },
    });
}

/// Saves a report for a crash; `None` when it cannot be written.
fn save_report(c: &Crash) -> Option<String> {
    store()?.save_report(c, &Env::detect()).ok()
}

/// `crabby -p`: one input, no interface.
fn print_mode(input: String, agents: &[Agent], cli: &Cli) -> ExitCode {
    deliberate("print");
    let input = if input == "-" {
        let mut text = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut text) {
            return fail(format!("could not read the input from stdin: {e}"));
        }
        text
    } else {
        input
    };
    if input.trim().is_empty() {
        return fail("the input is empty; give a message or a /command");
    }
    let parsed = match commands::parse(&input) {
        Ok(p) => p,
        Err(e) => return usage(e),
    };
    let known = cli.model.clone().or_else(settings::configured_model);
    // Nobody to ask: Fable above medium runs at medium.
    let effort = settings::effective_effort(known.as_deref(), cli.effort, false);
    if cli.effort > effort {
        eprintln!("crabby: no terminal to ask on, so this runs at medium effort (Fable ceiling)");
    }
    let (job, run_name) = match parsed {
        Input::Message(prompt) => (
            Job::Chat {
                prompt,
                session: None,
                mode: cli.mode,
                model: cli.model.clone(),
                effort,
            },
            None,
        ),
        Input::Command(Command::Help) => {
            println!("{}", commands::help());
            return ExitCode::SUCCESS;
        }
        Input::Command(Command::Agents(text)) => {
            let shown = commands::filter(agents, &text);
            if shown.is_empty() {
                return fail(format!("no agent matches `{text}`"));
            }
            let width = io::stdout()
                .is_terminal()
                .then(|| terminal_size::terminal_size().map_or(80, |(w, _)| w.0 as usize));
            // One write, error ignored: `crabby -p /agents | head` must not panic on a closed pipe.
            let _ = io::stdout()
                .lock()
                .write_all(display::render_list(&shown, width).as_bytes());
            return ExitCode::SUCCESS;
        }
        Input::Command(Command::Run { agent, task }) => {
            match commands::find_agent(agents, &agent) {
                Err(e) => return fail(e),
                Ok(a) => (
                    Job::Run {
                        agent: a.clone(),
                        task,
                        model: cli.model.clone(),
                        effort,
                        resume: None,
                    },
                    Some(a.name.clone()),
                ),
            }
        }
        Input::Command(Command::Plan(task)) => (Job::Plan { task }, None),
        Input::Command(
            Command::Model(_)
            | Command::Effort(_)
            | Command::Mode(_)
            | Command::Crashes
            | Command::CrashReports(_)
            | Command::Clear
            | Command::Quit,
        ) => {
            return usage(
                "that command only works in the chat; with -p use --model, --effort and --mode",
            );
        }
    };
    if io::stderr().is_terminal() {
        eprintln!("crabby: working via claude, this can take a few minutes...");
    }
    let shown_effort = if matches!(job, Job::Plan { .. }) {
        Some(Effort::Low)
    } else {
        effort
    };
    let mut exit = ExitCode::SUCCESS;
    crash::set_doing(Some(doing_of(&job)));
    execute(job, agents, &AtomicBool::new(false), |event| match event {
        JobEvent::Text(text) => {
            // Errors ignored: `crabby -p ... | head` must not panic on a closed pipe.
            let mut out = io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            if !text.ends_with('\n') {
                let _ = out.write_all(b"\n");
            }
            let _ = out.flush();
        }
        JobEvent::Tool { name, arg } => {
            let arg: String = arg.lines().next().unwrap_or("").chars().take(100).collect();
            eprintln!("crabby: {name} {arg}");
        }
        JobEvent::Session(_) => {}
        JobEvent::Failed(e) => exit = fail(e),
        // Writes the report and never asks: the filing is offered at the next chat.
        JobEvent::Crashed { crash, .. } => {
            exit = fail(format!(
                "the task crashed: {}",
                crash.message.lines().next().unwrap_or("")
            ));
            match save_report(&crash).and_then(|id| Some(store()?.path_of_report(&id))) {
                Some(p) => eprintln!("crabby: crash report saved to {}", p.display()),
                None => eprintln!("crabby: could not save a crash report"),
            }
        }
        JobEvent::Done { output, usage } => {
            if let Some(u) = &usage {
                eprintln!("{}", display::summary(u, shown_effort));
            }
            if let Some(name) = &run_name {
                let status = outcome::parse(&output).and_then(|o| o.status);
                if status.is_none() {
                    eprintln!(
                        "warning: {name} did not end with a result block, so its status is unknown (exit 3)"
                    );
                }
                exit = ExitCode::from(status_exit(status));
            }
        }
    });
    exit
}

/// Puts the terminal back: bracketed paste and mouse capture off, alternate screen
/// left, raw mode off, cursor shown.
fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    let _ = execute!(io::stdout(), Show);
}

/// The panic hook. On the main thread it stops the `claude` children, restores the
/// terminal before anything is printed, saves a report and prints one line. On a job
/// thread it only notes where the panic was: the job catches it and reports a crashed
/// task, and the interface stays up.
fn crash_hook(info: &panic::PanicHookInfo<'_>) {
    // A panic inside the hook would abort, so reporting is fenced off.
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        let location = info.location().map_or("unknown".to_string(), |l| {
            format!("{}:{}:{}", l.file(), l.line(), l.column())
        });
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let backtrace =
            crash::trim_backtrace(&std::backtrace::Backtrace::force_capture().to_string());
        crash::PANIC.with(|p| {
            *p.borrow_mut() = Some((location.clone(), message.clone(), backtrace.clone()));
        });
        if thread::current().name() != Some("main") {
            return;
        }
        runner::kill_all();
        if IN_CHAT.swap(false, Ordering::SeqCst) {
            restore_terminal();
        }
        let c = Crash {
            kind: crash::Kind::Panic,
            location: location.clone(),
            message: message.clone(),
            detail: Some(backtrace).filter(|b| !b.is_empty()),
            doing: crash::current_doing(),
        };
        match store().map(|s| (s, s.save_report(&c, &Env::detect()))) {
            Some((s, Ok(id))) => eprintln!(
                "crabby crashed: {message}\ncrabby: report saved to {}",
                s.path_of_report(&id).display()
            ),
            Some((_, Err(e))) => {
                eprintln!("crabby crashed ({message} at {location}); could not save a report: {e}")
            }
            None => eprintln!(
                "crabby crashed ({message} at {location}); no home folder to save a report in"
            ),
        }
    }));
}

/// How reports are posted: `gh`, or with `CRABBY_FILING=dry-run` a file under the base folder.
fn filer() -> Box<dyn Gh> {
    match (std::env::var("CRABBY_FILING").as_deref(), store()) {
        (Ok("dry-run"), Some(s)) => Box::new(crash::DryRun(s.base().join("dry-run-issues.json"))),
        _ => Box::new(crash::GhCli),
    }
}

/// Posts a report (or comments on the issue of the same crash) and tells the chat.
fn file_now(id: String, tx: &mpsc::Sender<Msg>) {
    let Some(s) = store() else { return };
    let result = s
        .send(&id, filer().as_ref(), &Env::detect())
        .map(|f| match f {
            Filed::Opened(u) => format!("crash report filed: {u}"),
            Filed::Commented(u) => {
                format!("this crash was already reported; added a comment to {u}")
            }
        });
    let _ = tx.send(Msg::Filed(result));
}

/// Adds the analysis to a report (one model call, 2 minutes at most), then files it or
/// hands the exact text back for the question.
fn prepare(id: String, then_file: bool, agents: Arc<Vec<Agent>>, tx: mpsc::Sender<Msg>) {
    thread::spawn(move || {
        let Some(s) = store() else { return };
        if let Some((meta, body)) = s.report(&id).filter(|(m, _)| !m.analysed) {
            let _ = meta;
            let cancel = Arc::new(AtomicBool::new(false));
            let timer = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(120));
                timer.store(true, Ordering::Relaxed);
            });
            let here = std::env::current_dir()
                .ok()
                .and_then(|d| crash::checkout_root(&d));
            let source = here.map(|_| agents.as_slice());
            if let Some((section, usage)) = crash::analyse(&body, source, &cancel) {
                let _ = s.add_analysis(&id, &section);
                if let Some(u) = usage {
                    let _ = tx.send(Msg::Note(format!(
                        "crash analysis: {}",
                        display::summary(&u, Some(Effort::Low))
                    )));
                }
            }
        }
        if then_file {
            file_now(id, &tx);
        } else if let Some((_, body)) = s.report(&id) {
            let _ = tx.send(Msg::Prepared { id, body });
        }
    });
}

/// The body of a job thread: runs the job, keeps its journal entry up to date and turns
/// a panic into a crashed task instead of taking the interface down.
fn run_job(
    id: u64,
    job: Job,
    agents: &[Agent],
    cancel: &AtomicBool,
    entry: Option<String>,
    tx: &mpsc::Sender<Msg>,
) {
    let mark = |state: Option<TaskState>, session: Option<String>| {
        if let (Some(s), Some(e)) = (store(), &entry) {
            let _ = s.update_entry(e, |x| {
                if let Some(st) = state {
                    x.state = st;
                }
                if session.is_some() {
                    x.session = session;
                }
            });
        }
    };
    let send = |event: JobEvent| {
        let event = match event {
            JobEvent::Session(s) => {
                mark(None, Some(s.clone()));
                JobEvent::Session(s)
            }
            JobEvent::Done { .. } => {
                mark(Some(TaskState::Finished), None);
                event
            }
            // A stop is finished; a known failure keeps the task offered for recovery.
            JobEvent::Failed(_) => {
                let stopped = cancel.load(Ordering::Relaxed);
                mark(
                    Some(if stopped {
                        TaskState::Finished
                    } else {
                        TaskState::Interrupted
                    }),
                    None,
                );
                event
            }
            JobEvent::Crashed { crash, .. } => {
                mark(Some(TaskState::Crashed), None);
                let report = save_report(&crash);
                JobEvent::Crashed { crash, report }
            }
            other => other,
        };
        let _ = tx.send(Msg::Job { id, event });
    };
    let run = panic::catch_unwind(AssertUnwindSafe(|| execute(job, agents, cancel, &send)));
    if let Err(p) = run {
        send(JobEvent::Crashed {
            crash: crash::from_panic(p.as_ref()),
            report: None,
        });
    }
    crash::set_doing(None);
}

/// Tells the user once that the journal could not be written (disk full, read-only home).
fn warn_once(tx: &mpsc::Sender<Msg>, warned: &mut bool, e: &io::Error) {
    if !std::mem::replace(warned, true) {
        let _ = tx.send(Msg::Note(format!(
            "could not write crash-recovery files ({e}); crabby keeps working, but a crash now cannot be recovered"
        )));
    }
}

fn chat(agents: Vec<Agent>, warnings: Vec<String>, cli: &Cli) -> ExitCode {
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let cwd = std::env::current_dir().map_or_else(|_| ".".into(), |p| p.display().to_string());
    let size = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let agents = Arc::new(agents);
    let mut st = State::new(
        agents.clone(),
        settings::configured_model(),
        cwd,
        color,
        size,
        &warnings,
    );
    st.mascot = color && mascot::truecolor();
    st.model = cli.model.as_ref().map(|m| m.to_ascii_lowercase());
    st.effort = cli.effort;
    st.mode = cli.mode;

    // What earlier windows left behind. Three failed start-ups in a row skip all of it.
    let mut pending = vec![];
    if let Some(s) = store() {
        st.consent = s.consent();
        let before = s.bump_startups().saturating_sub(1);
        let (found, reports) = if before >= journal::LOOP_LIMIT {
            (Found::default(), vec![])
        } else {
            let reports: Vec<_> = s
                .reports()
                .into_iter()
                .filter(|r| r.status == "pending")
                .take(3)
                .collect();
            (s.scan(std::process::id(), &journal::pid_alive), reports)
        };
        match journal::plan(
            before,
            found.conversation.is_some(),
            found.tasks.len(),
            reports.len(),
        ) {
            journal::Plan::Safe => {
                s.clear_startups();
                st.info(format!(
                    "crabby failed to start {before} times in a row, so recovery is skipped this time. Nothing was deleted: the journal and crash reports are still in {}.",
                    s.base().display()
                ));
            }
            journal::Plan::Offer => {
                if !reports.is_empty() && st.consent != Consent::Never {
                    st.info("a crash report from earlier was not sent yet");
                }
                st.offer(found);
                pending = reports;
            }
            journal::Plan::Nothing => {}
        }
    }

    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => return fail(format!("could not set up the terminal: {e}")),
    };
    IN_CHAT.store(true, Ordering::SeqCst);
    // try_init's hook restores the terminal on a panic in any thread; ours replaces it,
    // restores on the main thread only, and lets job threads report their own panics.
    panic::set_hook(Box::new(crash_hook));
    let _ = execute!(io::stdout(), EnableBracketedPaste);

    let result = event_loop(&mut terminal, &mut st, &agents, pending);
    IN_CHAT.store(false, Ordering::SeqCst);
    restore_terminal();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The snapshot stays, so the next start offers the conversation back.
        Err(e) => fail(e),
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    st: &mut State,
    agents: &Arc<Vec<Agent>>,
    pending: Vec<journal::ReportMeta>,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut current: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)> = None;
    let (owner, chat_id) = (std::process::id(), journal::new_id());
    let (started, mut last_at) = (Instant::now(), Instant::now());
    let (mut last_text, mut cleared, mut warned) = (String::new(), false, false);
    if st.consent != Consent::Never {
        for r in pending {
            prepare(
                r.id,
                st.consent == Consent::Always,
                agents.clone(),
                tx.clone(),
            );
        }
    }
    loop {
        terminal.draw(|f| {
            deliberate("draw");
            ui::draw(f, st)
        })?;
        deliberate("loop");
        let mut msgs = vec![];
        // The model calls run on their own threads, so this only waits for a key.
        if event::poll(Duration::from_millis(60))? {
            loop {
                match event::read()? {
                    Event::Key(k) => msgs.push(Msg::Key(k)),
                    Event::Paste(text) => msgs.push(Msg::Paste(text)),
                    Event::Resize(w, h) => msgs.push(Msg::Resize(w, h)),
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        } else {
            msgs.push(Msg::Tick);
        }
        msgs.extend(rx.try_iter());
        for msg in msgs {
            for effect in app::update(st, msg) {
                match effect {
                    Effect::Start { id, job, replaces } => {
                        crash::set_doing(Some(doing_of(&job)));
                        let entry = entry_of(&job, &st.cwd, replaces.as_deref());
                        // Written before the task starts, so a kill leaves a record of it.
                        let entry_id = store().and_then(|s| match s.save_entry(&entry) {
                            Ok(()) => Some(entry.id.clone()),
                            Err(e) => {
                                warn_once(&tx, &mut warned, &e);
                                None
                            }
                        });
                        let cancel = Arc::new(AtomicBool::new(false));
                        let (tx, agents, flag) = (tx.clone(), agents.clone(), cancel.clone());
                        let handle = thread::Builder::new()
                            .name("job".into())
                            .spawn(move || run_job(id, job, &agents, &flag, entry_id, &tx))?;
                        current = Some((cancel, handle));
                    }
                    Effect::Stop => {
                        if let Some((cancel, _)) = &current {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Effect::Prepare { id, then_file } => {
                        prepare(id, then_file, agents.clone(), tx.clone());
                    }
                    Effect::File { id } => {
                        let tx = tx.clone();
                        thread::spawn(move || file_now(id, &tx));
                    }
                    Effect::SkipReport { id } => {
                        if let Some(s) = store() {
                            let _ = s.set_status(&id, "skipped");
                        }
                    }
                    Effect::SetConsent(c) => {
                        if let Some(Err(e)) = store().map(|s| s.set_consent(c)) {
                            warn_once(&tx, &mut warned, &e);
                        }
                    }
                    Effect::ListCrashes => {
                        let text = store()
                            .map_or("no home folder, so no crash reports".into(), |s| {
                                journal::list_text(&s.reports())
                            });
                        let _ = tx.send(Msg::Note(text));
                    }
                    Effect::ForgetTask(id) => {
                        if let Some(s) = store() {
                            s.forget(&id);
                        }
                    }
                    Effect::ForgetChat(id) => {
                        if let Some(s) = store() {
                            s.forget_snapshot(&id);
                        }
                    }
                    Effect::Quit => {
                        // Wait for the kill, so no `claude` process outlives crabby.
                        if let Some((cancel, handle)) = current.take() {
                            cancel.store(true, Ordering::Relaxed);
                            let _ = handle.join();
                        }
                        // A clean exit leaves nothing to recover.
                        if let Some(s) = store() {
                            s.forget_snapshot(&chat_id);
                            s.clear_startups();
                        }
                        return Ok(());
                    }
                }
            }
        }
        if let Some(s) = store() {
            if !cleared && started.elapsed() > Duration::from_secs(3) {
                (cleared, _) = (true, s.clear_startups());
            }
            // The conversation and unsent input, at most once a second and only on change.
            if last_at.elapsed() > Duration::from_secs(1) {
                last_at = Instant::now();
                let snap = st.snapshot(&chat_id, owner);
                let text = snap.to_json().to_string();
                if text != last_text {
                    match s.save_snapshot(&snap) {
                        Ok(()) => last_text = text,
                        Err(e) => warn_once(&tx, &mut warned, &e),
                    }
                }
            }
        }
    }
}

fn main() -> ExitCode {
    if let Some(base) = Store::default_base() {
        let _ = STORE.set(Store::at(base));
    }
    panic::set_hook(Box::new(crash_hook));
    // A panic anywhere (also before the interface exists) ends here with the code for
    // "crabby could not do it"; the hook has already restored the terminal and saved a report.
    let code = panic::catch_unwind(real_main).unwrap_or(ExitCode::from(FAILED));
    runner::kill_all();
    code
}

fn real_main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // clap's own usage code is 2, which here means `blocked`.
            return ExitCode::from(if e.use_stderr() { USAGE } else { 0 });
        }
    };
    if let Some(m) = &cli.model {
        if let Err(e) = settings::check_model(m) {
            return usage(e);
        }
    }
    if cli.print.is_none() && !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return usage(
            "crabby opens an interactive chat and needs a terminal; use `crabby -p \"<message or /command>\"` in scripts",
        );
    }
    let (agents, warnings) = catalog::load(cli.agents_dir.as_deref());
    if agents.is_empty() {
        return match &cli.agents_dir {
            Some(d) => fail(format!(
                "no agents found in `{}`; --agents-dir must point at a `categories` folder holding <category>/<agent>.md files",
                d.display()
            )),
            None => fail("this build has no agents embedded; rebuild with the agents submodule checked out"),
        };
    }
    match cli.print.clone() {
        Some(input) => {
            for w in &warnings {
                eprintln!("warning: skipped {w}");
            }
            print_mode(input, &agents, &cli)
        }
        None => chat(agents, warnings, &cli),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_subcommands_are_gone_and_unknown_arguments_are_usage_errors() {
        for args in [
            vec!["crabby", "list"],
            vec!["crabby", "run", "sc-a", "task"],
            vec!["crabby", "plan", "task"],
            vec!["crabby", "--nope"],
        ] {
            let e = Cli::try_parse_from(args.clone()).err().expect("an error");
            assert!(e.use_stderr(), "{args:?}");
            assert!(e.to_string().contains("--help"), "{args:?}: {e}");
        }
    }

    #[test]
    fn print_takes_a_value_and_dash_means_stdin() {
        let cli = Cli::try_parse_from(["crabby", "-p", "hello", "--mode", "edit"])
            .ok()
            .unwrap();
        assert_eq!(cli.print.as_deref(), Some("hello"));
        assert_eq!(cli.mode, Mode::Edit);
        let cli = Cli::try_parse_from(["crabby", "-p", "-"]).ok().unwrap();
        assert_eq!(cli.print.as_deref(), Some("-"));
        let cli = Cli::try_parse_from(["crabby"]).ok().unwrap();
        assert!(cli.print.is_none());
        assert_eq!(cli.mode, Mode::Read);
    }

    #[test]
    fn statuses_keep_their_exit_codes() {
        assert_eq!(status_exit(Some(Status::Done)), 0);
        assert_eq!(status_exit(Some(Status::Partial)), 1);
        assert_eq!(status_exit(Some(Status::Blocked)), 2);
        assert_eq!(status_exit(None), 3);
    }
}
