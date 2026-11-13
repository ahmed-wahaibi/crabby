mod catalog;
mod outcome;
mod runner;

use catalog::Agent;
use clap::{Parser, Subcommand, ValueEnum};
use outcome::Status;
use runner::Event;
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
};

const AFTER_HELP: &str = "\
Examples:
  crabby list
  crabby list --category language-specialists
  crabby list | grep -i rust
  crabby run sc-rust-engineer \"make the parser report line numbers\"
  crabby run sc-code-reviewer - < task.md
  crabby run sc-debugger \"find the flaky test\" --model opus --effort high
  crabby run sc-cloud-architect \"sketch the queue\" --model fable --effort max

Effort ceiling: a run on Fable (`fable` or `claude-fable-*`) is capped at `medium`,
and without `--effort` it is passed `--effort medium`. Asking for more stops before `claude` starts and asks you to type `yes`; any other
answer, or no terminal to ask on, runs at `medium`. Without `--model` crabby reads
the model from ANTHROPIC_MODEL or Claude Code's settings files, and if it cannot
tell, it treats the run as possibly Fable.

Exit codes:
   0  the agent reported `done` (also: list, --help, --version)
   1  the agent reported `partial`
   2  the agent reported `blocked`
   3  the agent finished without a result block
   4  crabby could not run it (unknown agent, no agents found, `claude` missing or failed)
  64  usage error (bad flag or missing argument)";

#[derive(Parser)]
#[command(name = "crabby", version, about = "Orchestrator for the SuperCrab agents", after_help = AFTER_HELP)]
struct Cli {
    /// Read agents from this `categories` folder instead of the built-in set
    #[arg(long, global = true, value_name = "DIR")]
    agents_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the available agents
    ///
    /// On a terminal, agents are grouped by category and descriptions are cut to
    /// the window width. When piped, each line is `name<TAB>category<TAB>description`
    /// with the full description.
    List {
        /// Only show this category, e.g. `language-specialists`
        #[arg(long)]
        category: Option<String>,
    },
    /// Run one agent on a task in the current directory
    ///
    /// The agent runs as a headless `claude` process, so Claude Code must be
    /// installed. Its text goes to stdout as it arrives and its tool calls to
    /// stderr; the exit code reflects the status in its result block (see
    /// `crabby --help`).
    Run {
        /// Agent name as shown by `crabby list`, e.g. `sc-rust-engineer`
        agent: String,
        /// What the agent should do. Quote it; use `-` to read it from stdin,
        /// and put `--` before a task that starts with a dash
        #[arg(required = true)]
        task: Vec<String>,
        /// Model to run the agent on (defaults to your Claude Code setting)
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort. On Fable, above `medium` needs a typed `yes` (see `crabby --help`)
        #[arg(long, value_enum)]
        effort: Option<Effort>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    fn name(self) -> String {
        self.to_possible_value()
            .expect("no skipped variants")
            .get_name()
            .to_string()
    }
}

#[derive(Debug, PartialEq)]
enum Gate {
    Proceed,
    Ask,
    CapToMedium,
    /// Possibly Fable with no `--effort`: pass `medium` instead of Claude Code's default.
    DefaultMedium,
}

/// Decides whether a run may start at the requested effort. `model` is `None` when
/// it is unknown, which is treated as possibly Fable.
fn gate(model: Option<&str>, effort: Option<Effort>, terminal: bool) -> Gate {
    let fable = model.is_none_or(|m| {
        let m = m.trim().to_ascii_lowercase();
        m == "fable" || m.starts_with("claude-fable")
    });
    match effort {
        _ if !fable => Gate::Proceed,
        None => Gate::DefaultMedium,
        Some(e) if e <= Effort::Medium => Gate::Proceed,
        Some(_) if terminal => Gate::Ask,
        Some(_) => Gate::CapToMedium,
    }
}

/// `1234567` -> `1,234,567`.
fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The one-line run summary for stderr.
fn summary(u: &runner::Usage, effort: Option<Effort>) -> String {
    let mut parts = vec!["crabby:".to_string()];
    if !u.models.is_empty() {
        parts.push(u.models.join(", "));
        parts.push("·".into());
    }
    parts.push(format!(
        "effort {} · {} in / {} out",
        effort.map_or("default".into(), Effort::name),
        commas(u.input_tokens),
        commas(u.output_tokens)
    ));
    if u.cache_read_tokens > 0 {
        parts.push(format!("· {} cached", commas(u.cache_read_tokens)));
    }
    if u.cache_write_tokens > 0 {
        parts.push(format!("· {} written", commas(u.cache_write_tokens)));
    }
    if let Some(c) = u.cost_usd {
        parts.push(format!("· ${c:.4}"));
    }
    if let Some(ms) = u.duration_ms {
        parts.push(format!("· {}s", (ms + 500) / 1000));
    }
    parts.join(" ")
}

/// The model Claude Code would use when `--model` is not given: ANTHROPIC_MODEL, else
/// the first `model` in the local, project or user settings.
// shortcut: managed (enterprise) settings and an `env` block in settings are not read,
// so a model set only there is missed; `None` then makes the caller assume Fable.
fn configured_model() -> Option<String> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    let files = [
        PathBuf::from(".claude/settings.local.json"),
        PathBuf::from(".claude/settings.json"),
    ]
    .into_iter()
    .chain(home.map(|h| PathBuf::from(h).join(".claude/settings.json")));
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .or_else(|| {
            files.into_iter().find_map(|f| {
                let v: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(f).ok()?).ok()?;
                v["model"].as_str().map(String::from)
            })
        })
        .filter(|m| !m.trim().is_empty() && m != "default")
}

/// Asks on the terminal; true only for exactly `yes`.
fn confirm(agent: &str, model: Option<&str>, effort: Effort) -> bool {
    let which = match model {
        Some(m) => format!("model {m}"),
        None => "a model that could not be determined (it may be Fable)".into(),
    };
    eprint!(
        "crabby: {agent} is about to run on {which} at {} effort. Fable above medium effort costs more.\nType `yes` to run at {}, anything else runs at medium: ",
        effort.name(),
        effort.name()
    );
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok() && answer.trim_end_matches(['\r', '\n']) == "yes"
}

/// Exit code for anything crabby itself could not do.
const FAILED: u8 = 4;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(FAILED)
}

/// Renders the agent list. `width` is the terminal width; `None` means stdout is
/// not a terminal and gets one tab-separated line per agent.
fn render_list(agents: &[&Agent], width: Option<usize>) -> String {
    let mut out = String::new();
    let Some(width) = width else {
        for a in agents {
            out += &format!("{}\t{}\t{}\n", a.name, a.category, a.description);
        }
        return out;
    };
    let pad = agents.iter().map(|a| a.name.len()).max().unwrap_or(0);
    // Room left for the description; one column spare so a full line never wraps.
    let room = width.saturating_sub(pad + 5);
    let mut current = "";
    for a in agents {
        if a.category != current {
            if !current.is_empty() {
                out.push('\n');
            }
            current = &a.category;
            out += &format!("{current}\n");
        }
        if room < 20 {
            out += &format!("  {}\n", a.name); // too narrow for a useful description
        } else if a.description.chars().count() > room {
            let cut: String = a.description.chars().take(room - 1).collect();
            out += &format!("  {:<pad$}  {cut}…\n", a.name);
        } else {
            out += &format!("  {:<pad$}  {}\n", a.name, a.description);
        }
    }
    out
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // clap's own usage code is 2, which here means `blocked`.
            return ExitCode::from(if e.use_stderr() { 64 } else { 0 });
        }
    };
    let (agents, warnings) = catalog::load(cli.agents_dir.as_deref());
    for w in &warnings {
        eprintln!("warning: skipped {w}");
    }
    if agents.is_empty() {
        return match &cli.agents_dir {
            Some(d) => fail(format!(
                "no agents found in `{}`; --agents-dir must point at a `categories` folder holding <category>/<agent>.md files",
                d.display()
            )),
            None => fail("this build has no agents embedded; rebuild with the agents submodule checked out"),
        };
    }

    match cli.command {
        Cmd::List { category } => {
            let shown: Vec<&Agent> = agents
                .iter()
                .filter(|a| category.as_ref().is_none_or(|c| *c == a.category))
                .collect();
            if shown.is_empty() {
                let mut cats: Vec<&str> = agents.iter().map(|a| a.category.as_str()).collect();
                cats.dedup();
                return fail(format!(
                    "no category named `{}`; categories: {}",
                    category.unwrap_or_default(),
                    cats.join(", ")
                ));
            }
            let stdout = io::stdout();
            let width = stdout
                .is_terminal()
                .then(|| terminal_size::terminal_size().map_or(80, |(w, _)| w.0 as usize));
            // One write, error ignored: `crabby list | head` must not panic on a closed pipe.
            let _ = stdout
                .lock()
                .write_all(render_list(&shown, width).as_bytes());
            ExitCode::SUCCESS
        }
        Cmd::Run {
            agent,
            task,
            model,
            effort,
        } => {
            let Some(agent) = agents.iter().find(|a| a.name == agent) else {
                let key = agent.trim_start_matches("sc-");
                let near: Vec<&str> = agents
                    .iter()
                    .filter(|a| !key.is_empty() && a.name.contains(key))
                    .map(|a| a.name.as_str())
                    .take(5)
                    .collect();
                return fail(if near.is_empty() {
                    format!("no agent named `{agent}`; run `crabby list` to see the names")
                } else {
                    format!(
                        "no agent named `{agent}`; did you mean: {}",
                        near.join(", ")
                    )
                });
            };
            let from_stdin = task == ["-"];
            let task = if from_stdin {
                let mut t = String::new();
                if let Err(e) = io::stdin().read_to_string(&mut t) {
                    return fail(format!("could not read the task from stdin: {e}"));
                }
                t
            } else {
                task.join(" ")
            };
            if task.trim().is_empty() {
                return fail("the task is empty; say what the agent should do");
            }
            let known = model.clone().or_else(configured_model);
            let terminal = !from_stdin && io::stdin().is_terminal() && io::stderr().is_terminal();
            let effort = match (gate(known.as_deref(), effort, terminal), effort) {
                (Gate::Ask, Some(e)) if !confirm(&agent.name, known.as_deref(), e) => {
                    eprintln!("crabby: running at medium effort");
                    Some(Effort::Medium)
                }
                (Gate::CapToMedium, _) => {
                    eprintln!("crabby: no terminal to ask on, so this runs at medium effort (Fable ceiling)");
                    Some(Effort::Medium)
                }
                (Gate::DefaultMedium, _) => Some(Effort::Medium),
                _ => effort,
            };
            if io::stderr().is_terminal() {
                eprintln!(
                    "crabby: running {} via claude, this can take a few minutes...",
                    agent.name
                );
            }
            let shown = |event| match event {
                Event::Text(text) => {
                    // Errors ignored: `crabby run ... | head` must not panic on a closed pipe.
                    let mut out = io::stdout().lock();
                    let _ = out.write_all(text.as_bytes());
                    if !text.ends_with('\n') {
                        let _ = out.write_all(b"\n");
                    }
                    let _ = out.flush();
                }
                Event::Tool { name, arg } => {
                    let arg: String = arg.lines().next().unwrap_or("").chars().take(100).collect();
                    eprintln!("crabby: {name} {arg}");
                }
            };
            let (output, usage) = match runner::run(
                agent,
                &task,
                model.as_deref(),
                effort.map(Effort::name).as_deref(),
                shown,
            ) {
                Ok(o) => o,
                Err(e) => return fail(e),
            };
            if let Some(u) = &usage {
                eprintln!("{}", summary(u, effort));
            }
            match outcome::parse(&output).and_then(|o| o.status) {
                Some(Status::Done) => ExitCode::SUCCESS,
                Some(Status::Partial) => ExitCode::from(1),
                Some(Status::Blocked) => ExitCode::from(2),
                None => {
                    eprintln!(
                        "warning: {} did not end with a result block, so its status is unknown (exit 3)",
                        agent.name
                    );
                    ExitCode::from(3)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, category: &str, description: &str) -> Agent {
        Agent {
            name: name.into(),
            description: description.into(),
            category: category.into(),
            tools: vec![],
            body: String::new(),
        }
    }

    #[test]
    fn gate_covers_every_branch() {
        use Effort::*;
        let g = gate;
        // Not Fable, or nothing above medium asked for: go.
        assert_eq!(g(Some("opus"), Some(Max), true), Gate::Proceed);
        assert_eq!(g(Some("claude-opus-5-5"), Some(High), false), Gate::Proceed);
        assert_eq!(g(Some("opus"), None, true), Gate::Proceed);
        assert_eq!(g(Some("fable"), Some(Low), false), Gate::Proceed);
        assert_eq!(g(Some("fable"), Some(Medium), false), Gate::Proceed);
        assert_eq!(g(None, Some(Medium), false), Gate::Proceed);
        // Fable above medium: ask on a terminal, else cap.
        for e in [High, Xhigh, Max] {
            assert_eq!(g(Some("fable"), Some(e), true), Gate::Ask);
            assert_eq!(g(Some("fable"), Some(e), false), Gate::CapToMedium);
        }
        // Names are matched case-insensitively; look-alikes are not Fable.
        assert_eq!(g(Some("Claude-FABLE-5-2"), Some(High), true), Gate::Ask);
        assert_eq!(g(Some("FABLE"), Some(High), false), Gate::CapToMedium);
        assert_eq!(g(Some("my-fable"), Some(High), true), Gate::Proceed);
        // Fable or unknown with no effort: pass medium explicitly, no prompt.
        assert_eq!(g(Some("fable"), None, true), Gate::DefaultMedium);
        assert_eq!(
            g(Some("claude-fable-5-2"), None, false),
            Gate::DefaultMedium
        );
        assert_eq!(g(None, None, true), Gate::DefaultMedium);
        // Unknown model counts as possibly Fable.
        assert_eq!(g(None, Some(High), true), Gate::Ask);
        assert_eq!(g(None, Some(Max), false), Gate::CapToMedium);
    }

    #[test]
    fn summary_line_shows_what_the_event_gave() {
        let u = runner::Usage {
            input_tokens: 1204,
            output_tokens: 310,
            cache_read_tokens: 18400,
            cost_usd: Some(0.00312),
            duration_ms: Some(11600),
            models: vec!["claude-haiku-5-5".into()],
            ..Default::default()
        };
        assert_eq!(
            summary(&u, Some(Effort::Low)),
            "crabby: claude-haiku-5-5 · effort low · 1,204 in / 310 out · 18,400 cached · $0.0031 · 12s"
        );
        assert_eq!(
            summary(&runner::Usage::default(), None),
            "crabby: effort default · 0 in / 0 out"
        );
    }

    #[test]
    fn list_is_tab_separated_when_piped_and_fits_the_terminal_otherwise() {
        let long = "x".repeat(200);
        let (a, b) = (agent("sc-a", "one", "Short."), agent("sc-bb", "two", &long));
        let agents = [&a, &b];

        assert_eq!(
            render_list(&agents, None),
            format!("sc-a\tone\tShort.\nsc-bb\ttwo\t{long}\n")
        );

        let wide = render_list(&agents, Some(60));
        assert!(
            wide.starts_with("one\n  sc-a   Short.\n\ntwo\n  sc-bb  xxx"),
            "{wide}"
        );
        assert!(wide.lines().all(|l| l.chars().count() < 60), "{wide}");
        assert!(wide.trim_end().ends_with('…'));

        assert_eq!(
            render_list(&agents, Some(20)),
            "one\n  sc-a\n\ntwo\n  sc-bb\n"
        );
    }
}
