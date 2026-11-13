mod app;
mod catalog;
mod commands;
mod display;
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
use outcome::Status;
use ratatui::crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
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
        mpsc, Arc,
    },
    thread,
    time::Duration,
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
/mode, /clear and /quit. A plain message is a conversation with one Claude Code
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
   4  crabby could not do it (unknown agent, no agents found, `claude` missing or failed)
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

/// Runs a job to its end on the calling thread, reporting through `emit`. The last
/// event is always `Done` or `Failed`.
fn execute(job: Job, agents: &[Agent], cancel: &AtomicBool, mut emit: impl FnMut(JobEvent)) {
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
        } => runner::run(
            &agent,
            &task,
            model.as_deref(),
            effort.map(Effort::name).as_deref(),
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
        Err(e) => JobEvent::Failed(e.to_string()),
    });
}

/// `crabby -p`: one input, no interface.
fn print_mode(input: String, agents: &[Agent], cli: &Cli) -> ExitCode {
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

/// Puts the terminal back: bracketed paste off, alternate screen left, raw mode off.
fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
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

    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => return fail(format!("could not set up the terminal: {e}")),
    };
    // try_init's hook already restores the terminal on a panic in this thread; this
    // one adds bracketed paste, and keeps job threads (which report their own panics)
    // from tearing the screen down or printing over it.
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if thread::current().name() == Some("main") {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            previous(info);
        }
    }));
    let _ = execute!(io::stdout(), EnableBracketedPaste);

    let result = event_loop(&mut terminal, &mut st, &agents);
    restore_terminal();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    st: &mut State,
    agents: &Arc<Vec<Agent>>,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut current: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)> = None;
    loop {
        terminal.draw(|f| ui::draw(f, st))?;
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
                    Effect::Start { id, job } => {
                        let cancel = Arc::new(AtomicBool::new(false));
                        let (tx, agents, flag) = (tx.clone(), agents.clone(), cancel.clone());
                        let handle =
                            thread::Builder::new().name("job".into()).spawn(move || {
                                let send = |event| {
                                    let _ = tx.send(Msg::Job { id, event });
                                };
                                let run = panic::catch_unwind(AssertUnwindSafe(|| {
                                    execute(job, &agents, &flag, send)
                                }));
                                if let Err(p) = run {
                                    let why = p
                                        .downcast_ref::<&str>()
                                        .map(|s| s.to_string())
                                        .or_else(|| p.downcast_ref::<String>().cloned())
                                        .unwrap_or_default();
                                    send(JobEvent::Failed(format!("internal error: {why}")));
                                }
                            })?;
                        current = Some((cancel, handle));
                    }
                    Effect::Stop => {
                        if let Some((cancel, _)) = &current {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Effect::Quit => {
                        // Wait for the kill, so no `claude` process outlives crabby.
                        if let Some((cancel, handle)) = current.take() {
                            cancel.store(true, Ordering::Relaxed);
                            let _ = handle.join();
                        }
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn main() -> ExitCode {
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
