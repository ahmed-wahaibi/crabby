mod catalog;
mod outcome;
mod runner;

use catalog::Agent;
use clap::{Parser, Subcommand};
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
    },
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
        Cmd::Run { agent, task, model } => {
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
            let task = if task == ["-"] {
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
            let output = match runner::run(agent, &task, model.as_deref(), shown) {
                Ok(o) => o,
                Err(e) => return fail(e),
            };
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
