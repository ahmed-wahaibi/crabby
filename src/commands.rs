//! Slash commands: parsing, completion and agent lookup. No terminal or model code.

use crate::catalog::Agent;

/// Name, usage and what it does, in the order `/help` lists them.
pub const COMMANDS: [(&str, &str, &str); 11] = [
    ("help", "/help", "list the commands"),
    (
        "agents",
        "/agents [text]",
        "list agents, filtered by text in the name, category or description",
    ),
    (
        "run",
        "/run <agent> <task>",
        "run one agent on a task, with the tools in its header",
    ),
    (
        "plan",
        "/plan <task>",
        "let the router pick agents; nothing is executed",
    ),
    (
        "model",
        "/model [name]",
        "show or set the model for messages and /run",
    ),
    ("effort", "/effort [level]", "show or set the effort"),
    (
        "mode",
        "/mode [read|edit]",
        "show or set what a plain conversation may do",
    ),
    (
        "crashes",
        "/crashes",
        "list recent crash reports with their issue links",
    ),
    (
        "crash-reports",
        "/crash-reports [ask|always|never]",
        "show or set whether crash reports are filed as GitHub issues",
    ),
    ("clear", "/clear", "start a new conversation"),
    ("quit", "/quit", "exit"),
];

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Agents(String),
    Run { agent: String, task: String },
    Plan(String),
    Model(Option<String>),
    Effort(Option<String>),
    Mode(Option<String>),
    Crashes,
    CrashReports(Option<String>),
    Clear,
    Quit,
}

#[derive(Debug, PartialEq)]
pub enum Input {
    Message(String),
    Command(Command),
}

/// Reads one input. Text that does not start with `/word` is a message, so a path
/// such as `/usr/bin/env` or `//comment` goes to the model as typed.
pub fn parse(input: &str) -> Result<Input, String> {
    let input = input.trim();
    let Some(rest) = input.strip_prefix('/') else {
        return Ok(Input::Message(input.to_string()));
    };
    let (name, arg) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(n, a)| (n, a.trim()));
    if !name.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        return Ok(Input::Message(input.to_string()));
    }
    let opt = || (!arg.is_empty()).then(|| arg.to_string());
    let need = |usage: &str| {
        if arg.is_empty() {
            Err(format!("usage: {usage}"))
        } else {
            Ok(arg.to_string())
        }
    };
    let name = name.to_ascii_lowercase();
    let command = match name.as_str() {
        "" | "help" => Command::Help,
        "agents" => Command::Agents(arg.to_string()),
        "run" => {
            let task = need("/run <agent> <task>")?;
            let (agent, task) = task
                .split_once(char::is_whitespace)
                .ok_or("usage: /run <agent> <task>")?;
            Command::Run {
                agent: agent.to_string(),
                task: task.trim().to_string(),
            }
        }
        "plan" => Command::Plan(need("/plan <task>")?),
        "model" => Command::Model(opt()),
        "effort" => Command::Effort(opt()),
        "mode" => Command::Mode(opt()),
        "crashes" => Command::Crashes,
        "crash-reports" => Command::CrashReports(opt()),
        "clear" => Command::Clear,
        "quit" | "exit" => Command::Quit,
        _ => {
            let near: Vec<String> = matching(&format!("/{name}"))
                .iter()
                .map(|c| format!("/{}", c.0))
                .collect();
            return Err(if near.is_empty() {
                format!("unknown command `/{name}`; /help lists the commands")
            } else {
                format!(
                    "unknown command `/{name}`; did you mean {}?",
                    near.join(", ")
                )
            });
        }
    };
    Ok(Input::Command(command))
}

/// The commands whose name starts with what is typed after the `/`. Empty unless the
/// input is a single `/word` still being typed.
pub fn matching(input: &str) -> Vec<&'static (&'static str, &'static str, &'static str)> {
    let Some(typed) = input.strip_prefix('/') else {
        return vec![];
    };
    if typed.contains(char::is_whitespace) {
        return vec![];
    }
    let typed = typed.to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|c| c.0.starts_with(&typed))
        .collect()
}

/// Tab completion: the one match, or the longest prefix the matches share.
/// `None` when there is nothing to add.
pub fn complete(input: &str) -> Option<String> {
    let names: Vec<&str> = matching(input).iter().map(|c| c.0).collect();
    let first = *names.first()?;
    let common = names.iter().fold(first.len(), |n, name| {
        first
            .bytes()
            .zip(name.bytes())
            .take(n)
            .take_while(|(a, b)| a == b)
            .count()
    });
    let done = if names.len() == 1 {
        format!("/{first} ")
    } else {
        format!("/{}", &first[..common])
    };
    (done.len() > input.len()).then_some(done)
}

/// `/help` text, one command per line.
pub fn help() -> String {
    let pad = COMMANDS.iter().map(|c| c.1.len()).max().unwrap_or(0);
    let mut out: String = COMMANDS
        .iter()
        .map(|(_, usage, what)| format!("  {usage:<pad$}  {what}\n"))
        .collect();
    out += "Anything else is a message to Claude Code. Enter sends, Alt+Enter adds a line, Up/Down recall, PageUp/PageDown scroll, Esc stops a reply.";
    out
}

/// Agents whose name, category or description contains `text` (any case); all when empty.
pub fn filter<'a>(agents: &'a [Agent], text: &str) -> Vec<&'a Agent> {
    let t = text.trim().to_lowercase();
    agents
        .iter()
        .filter(|a| {
            [&a.name, &a.category, &a.description]
                .iter()
                .any(|f| f.to_lowercase().contains(&t))
        })
        .collect()
}

/// The agent called `name`, or an error that says what was close.
pub fn find_agent<'a>(agents: &'a [Agent], name: &str) -> Result<&'a Agent, String> {
    if let Some(a) = agents.iter().find(|a| a.name == name) {
        return Ok(a);
    }
    let key = name.trim_start_matches("sc-");
    let near: Vec<&str> = agents
        .iter()
        .filter(|a| !key.is_empty() && a.name.contains(key))
        .map(|a| a.name.as_str())
        .take(5)
        .collect();
    Err(if near.is_empty() {
        format!("no agent named `{name}`; /agents lists the names")
    } else {
        format!("no agent named `{name}`; did you mean: {}", near.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Command::*;

    fn cmd(s: &str) -> Command {
        match parse(s).unwrap() {
            Input::Command(c) => c,
            m => panic!("{m:?}"),
        }
    }

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
    fn every_command_parses() {
        assert_eq!(cmd("/help"), Help);
        assert_eq!(cmd("/"), Help);
        assert_eq!(cmd("/agents"), Agents(String::new()));
        assert_eq!(cmd("/agents  rust  "), Agents("rust".into()));
        assert_eq!(
            cmd("/run sc-a fix the\nparser"),
            Run {
                agent: "sc-a".into(),
                task: "fix the\nparser".into()
            }
        );
        assert_eq!(cmd("/plan add tests"), Plan("add tests".into()));
        assert_eq!(cmd("/model"), Model(None));
        assert_eq!(cmd("/model opus"), Model(Some("opus".into())));
        assert_eq!(cmd("/effort high"), Effort(Some("high".into())));
        assert_eq!(cmd("/mode edit"), Mode(Some("edit".into())));
        assert_eq!(cmd("/clear"), Clear);
        assert_eq!(cmd("/crashes"), Crashes);
        assert_eq!(cmd("/crash-reports"), CrashReports(None));
        assert_eq!(
            cmd("/crash-reports never"),
            CrashReports(Some("never".into()))
        );
        assert_eq!(cmd("/QUIT"), Quit);
    }

    #[test]
    fn missing_arguments_and_unknown_commands_are_errors() {
        for bad in ["/run", "/run sc-a", "/run sc-a   ", "/plan", "/plan  "] {
            assert!(parse(bad).unwrap_err().starts_with("usage:"), "{bad}");
        }
        assert!(parse("/hepl").unwrap_err().contains("unknown command"));
        assert!(parse("/mod").unwrap_err().contains("did you mean /model"));
    }

    #[test]
    fn text_that_is_not_a_command_is_a_message() {
        for m in [
            "hello",
            "  hi  ",
            "/usr/bin/env is missing",
            "//comment",
            "/etc/hosts?",
        ] {
            assert_eq!(parse(m).unwrap(), Input::Message(m.trim().into()), "{m}");
        }
    }

    #[test]
    fn typing_a_slash_lists_matches_and_tab_completes() {
        assert_eq!(matching("/").len(), COMMANDS.len());
        assert_eq!(matching("/m").len(), 2); // model, mode
        assert!(matching("/run x").is_empty());
        assert!(matching("hello").is_empty());
        assert_eq!(complete("/he").as_deref(), Some("/help "));
        assert_eq!(complete("/mo").as_deref(), Some("/mode")); // `mode` is a prefix of `model`
        assert_eq!(complete("/mode").as_deref(), None);
        assert_eq!(complete("/e").as_deref(), Some("/effort "));
        assert_eq!(complete("/").as_deref(), None);
        assert_eq!(complete("/q").as_deref(), Some("/quit "));
        assert_eq!(complete("/zzz"), None);
    }

    #[test]
    fn agents_filter_on_name_category_and_description() {
        let all = [
            agent("sc-rust-engineer", "languages", "Writes Rust."),
            agent(
                "sc-docs",
                "writing",
                "Keeps the README in line with RUST code.",
            ),
            agent("sc-go", "languages", "Go."),
        ];
        assert_eq!(filter(&all, "").len(), 3);
        assert_eq!(filter(&all, "rust").len(), 2);
        assert_eq!(filter(&all, "LANGUAGES").len(), 2);
        assert!(filter(&all, "zzz").is_empty());
    }

    #[test]
    fn an_unknown_agent_gets_a_suggestion() {
        let all = [agent("sc-rust-engineer", "c", "d")];
        assert!(find_agent(&all, "sc-rust-engineer").is_ok());
        let e = find_agent(&all, "rust").unwrap_err();
        assert!(e.contains("did you mean: sc-rust-engineer"), "{e}");
        assert!(find_agent(&all, "nope").unwrap_err().contains("/agents"));
    }
}
