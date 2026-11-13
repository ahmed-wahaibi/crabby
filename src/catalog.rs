//! The agent catalogue: every `categories/<NN-category>/<name>.md` file, parsed.

use include_dir::{include_dir, Dir};
use std::{fs, path::Path};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/agents/categories");

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub category: String,
    pub tools: Vec<String>,
    /// Everything after the frontmatter: the agent's instructions.
    pub body: String,
}

/// Parses one agent file. `stem` is the file name without `.md`.
pub fn parse(stem: &str, text: &str) -> Result<Agent, String> {
    // Editors on Windows may save a byte-order mark in front of the first `---`.
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n").ok_or("missing frontmatter")?;
    let (header, body) = rest
        .split_once("\n---\n")
        .ok_or("unterminated frontmatter")?;

    let field = |key: &str| -> Result<String, String> {
        header
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(|v| unquote(v.trim()).to_string())
            .filter(|v| !v.is_empty())
            .ok_or(format!("missing `{key}`"))
    };

    let name = field("name")?;
    if name != stem {
        return Err(format!("name `{name}` does not match file name"));
    }
    if !body.lines().any(|l| l.trim() == "## Result") {
        return Err("no `## Result` section".into());
    }
    Ok(Agent {
        name,
        description: field("description")?,
        category: field("category")?,
        tools: field("tools")?
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        body: body.trim().to_string(),
    })
}

/// Strips one pair of matching quotes, leaving quotes inside the value alone.
fn unquote(v: &str) -> &str {
    ['"', '\'']
        .iter()
        .find_map(|&q| v.strip_prefix(q)?.strip_suffix(q))
        .unwrap_or(v)
}

fn is_md(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()) == Some("md")
}

/// Loads the built-in agents, or those under `dir` (a `categories` folder) when given.
/// Invalid files are skipped and reported in the second value.
pub fn load(dir: Option<&Path>) -> (Vec<Agent>, Vec<String>) {
    let mut files: Vec<(String, String)> = Vec::new(); // (path, text)
    let (mut agents, mut warnings): (Vec<Agent>, Vec<String>) = (Vec::new(), Vec::new());
    match dir {
        None => {
            for f in EMBEDDED
                .dirs()
                .flat_map(|cat| cat.files())
                .filter(|f| is_md(f.path()))
            {
                let path = f.path().display().to_string();
                match f.contents_utf8() {
                    Some(t) => files.push((path, t.to_string())),
                    None => warnings.push(format!("{path}: not valid UTF-8")),
                }
            }
        }
        Some(root) => {
            for cat in fs::read_dir(root).into_iter().flatten().flatten() {
                for f in fs::read_dir(cat.path()).into_iter().flatten().flatten() {
                    let path = f.path();
                    if !is_md(&path) {
                        continue;
                    }
                    match fs::read_to_string(&path) {
                        Ok(t) => files.push((path.display().to_string(), t)),
                        Err(e) => warnings.push(format!("{}: {e}", path.display())),
                    }
                }
            }
        }
    }

    for (path, text) in files {
        let stem = Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        match parse(stem, &text) {
            // `run` picks an agent by name, so a second one could never be reached.
            Ok(a) if agents.iter().any(|b| b.name == a.name) => {
                warnings.push(format!("{path}: duplicate agent name `{}`", a.name))
            }
            Ok(a) => agents.push(a),
            Err(e) => warnings.push(format!("{path}: {e}")),
        }
    }
    agents.sort_by(|a, b| (&a.category, &a.name).cmp(&(&b.category, &b.name)));
    (agents, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: sc-x\ndescription: Does x.\ncategory: c\ntools: Read, Grep\nmodel: inherit\n---\n\nBody.\n\n## Result\n";

    #[test]
    fn parses_a_valid_agent() {
        let a = parse("sc-x", GOOD).unwrap();
        assert_eq!(a.tools, ["Read", "Grep"]);
        assert_eq!(a.category, "c");
        assert!(a.body.starts_with("Body."));
    }

    #[test]
    fn accepts_crlf_and_rejects_bad_files() {
        assert!(parse("sc-x", &GOOD.replace('\n', "\r\n")).is_ok());
        assert!(parse("other", GOOD).is_err()); // name mismatch
        assert!(parse("sc-x", &GOOD.replace("## Result", "")).is_err());
        assert!(parse("sc-x", "no frontmatter").is_err());
    }

    #[test]
    fn frontmatter_edge_cases() {
        // A key that is a prefix of another key must not match the longer one.
        let a = parse(
            "sc-x",
            &GOOD.replace("name: sc-x", "names: wrong\nname: sc-x"),
        )
        .unwrap();
        assert_eq!(a.name, "sc-x");
        assert!(parse("sc-x", &GOOD.replace("name: sc-x", "names: sc-x")).is_err());

        // One pair of quotes comes off; quotes and colons inside the value stay.
        let quoted = |d: &str| {
            parse("sc-x", &GOOD.replace("Does x.", d))
                .unwrap()
                .description
        };
        assert_eq!(
            quoted(r#""Use for: the "big" jobs""#),
            r#"Use for: the "big" jobs"#
        );
        assert_eq!(quoted("'single'"), "single");
        assert_eq!(quoted(r#"say "hi""#), r#"say "hi""#);

        // Byte-order mark in front, empty entries in the tool list.
        let text = format!("\u{feff}{}", GOOD.replace("Read, Grep", "Read,, Grep,"));
        assert_eq!(parse("sc-x", &text).unwrap().tools, ["Read", "Grep"]);

        assert!(parse("sc-x", &GOOD.replace("Does x.", "\"\"")).is_err()); // empty value
        assert!(parse("sc-x", "---\nname: sc-x\n").is_err()); // unterminated
    }

    #[test]
    fn load_reports_duplicates_and_unreadable_files() {
        let root = std::env::temp_dir().join(format!("crabby-test-{}", std::process::id()));
        for cat in ["01-a", "02-b"] {
            fs::create_dir_all(root.join(cat)).unwrap();
            fs::write(root.join(cat).join("sc-x.md"), GOOD).unwrap();
        }
        fs::write(root.join("01-a/sc-bin.md"), [0xff, 0xfe]).unwrap();
        fs::write(root.join("01-a/notes.txt"), "ignored").unwrap();
        let (agents, warnings) = load(Some(&root));
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
    }

    #[test]
    fn every_built_in_agent_is_valid() {
        let (agents, warnings) = load(None);
        assert!(warnings.is_empty(), "{warnings:#?}");
        assert!(agents.len() >= 152, "only {} agents embedded", agents.len());
    }
}
