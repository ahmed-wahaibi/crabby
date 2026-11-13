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
    let text = text.replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n").ok_or("missing frontmatter")?;
    let (header, body) = rest.split_once("\n---\n").ok_or("unterminated frontmatter")?;

    let field = |key: &str| -> Result<String, String> {
        header
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(|v| v.trim().trim_matches('"').to_string())
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
        tools: field("tools")?.split(',').map(|t| t.trim().to_string()).collect(),
        body: body.trim().to_string(),
    })
}

/// Loads the built-in agents, or those under `dir` (a `categories` folder) when given.
/// Invalid files are skipped and reported in the second value.
pub fn load(dir: Option<&Path>) -> (Vec<Agent>, Vec<String>) {
    let mut files: Vec<(String, String)> = Vec::new(); // (path, text)
    match dir {
        None => {
            for cat in EMBEDDED.dirs() {
                for f in cat.files() {
                    if let Some(t) = f.contents_utf8() {
                        files.push((f.path().display().to_string(), t.to_string()));
                    }
                }
            }
        }
        Some(root) => {
            for cat in fs::read_dir(root).into_iter().flatten().flatten() {
                for f in fs::read_dir(cat.path()).into_iter().flatten().flatten() {
                    if let Ok(t) = fs::read_to_string(f.path()) {
                        files.push((f.path().display().to_string(), t));
                    }
                }
            }
        }
    }

    let (mut agents, mut warnings) = (Vec::new(), Vec::new());
    for (path, text) in files {
        let p = Path::new(&path);
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        match parse(stem, &text) {
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
    fn every_built_in_agent_is_valid() {
        let (agents, warnings) = load(None);
        assert!(warnings.is_empty(), "{warnings:#?}");
        assert!(agents.len() >= 152, "only {} agents embedded", agents.len());
    }
}
