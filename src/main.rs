mod catalog;
mod outcome;
mod runner;

use clap::{Parser, Subcommand};
use outcome::Status;
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "crabby", version, about = "Orchestrator for the SuperCrab agents")]
struct Cli {
    /// Read agents from this `categories` folder instead of the built-in set
    #[arg(long, global = true)]
    agents_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the available agents
    List {
        /// Only show this category, e.g. `language-specialists`
        #[arg(long)]
        category: Option<String>,
    },
    /// Run one agent on a task in the current directory
    Run {
        agent: String,
        /// What the agent should do
        #[arg(required = true)]
        task: Vec<String>,
        /// Model to run the agent on (defaults to your Claude Code setting)
        #[arg(long)]
        model: Option<String>,
    },
}

/// Exit codes: 0 done, 1 partial, 2 blocked, 3 no result block, 4 could not run.
fn main() -> ExitCode {
    let cli = Cli::parse();
    let (agents, warnings) = catalog::load(cli.agents_dir.as_deref());
    for w in &warnings {
        eprintln!("warning: skipped {w}");
    }

    match cli.command {
        Cmd::List { category } => {
            let mut current = "";
            for a in agents.iter().filter(|a| category.as_ref().is_none_or(|c| *c == a.category)) {
                if a.category != current {
                    current = &a.category;
                    println!("\n{current}");
                }
                println!("  {:<38} {}", a.name, a.description);
            }
            ExitCode::SUCCESS
        }
        Cmd::Run { agent, task, model } => {
            let Some(agent) = agents.iter().find(|a| a.name == agent) else {
                eprintln!("error: no agent named `{agent}` (see `crabby list`)");
                return ExitCode::from(4);
            };
            let output = match runner::run(agent, &task.join(" "), model.as_deref()) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(4);
                }
            };
            println!("{output}");
            match outcome::parse(&output).and_then(|o| o.status) {
                Some(Status::Done) => ExitCode::SUCCESS,
                Some(Status::Partial) => ExitCode::from(1),
                Some(Status::Blocked) => ExitCode::from(2),
                None => {
                    eprintln!("warning: {} did not end with a result block", agent.name);
                    ExitCode::from(3)
                }
            }
        }
    }
}
