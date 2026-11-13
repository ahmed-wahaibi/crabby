//! Parses the `## Result` block every agent ends its final message with.

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Status {
    Done,
    Partial,
    Blocked,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub status: Option<Status>,
    pub summary: String,
    pub changes: String,
    pub verification: String,
    pub follow_ups: String,
}

/// Reads the last `## Result` block in `output`. `None` when there is no block
/// or it has no recognisable status.
pub fn parse(output: &str) -> Option<Outcome> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.iter().rposition(|l| l.trim() == "## Result")?;

    let mut out = Outcome::default();
    let mut current: Option<&mut String> = None;
    let mut base: Option<usize> = None; // indent of the block's own bullets
    for raw in &lines[start + 1..] {
        let line = raw.trim();
        let indent = raw.len() - raw.trim_start().len();
        if line.starts_with("```") && base.is_none() {
            continue; // a fence opened between the heading and the fields
        }
        if line.starts_with("```") || line.starts_with('#') {
            break;
        }
        // A deeper bullet (`  - a.rs: why`) is part of the value above it, not a key.
        let entry = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .filter(|_| base.is_none_or(|b| indent <= b))
            .and_then(|l| l.split_once(':'));
        if let Some((key, value)) = entry {
            base.get_or_insert(indent);
            // Models often bold the keys: `- **status:** done`, `- **status**: done`.
            let value = value.trim().trim_start_matches("**").trim();
            current = match key.trim().trim_matches(['*', '`']).to_lowercase().as_str() {
                "status" => {
                    let word = value.trim_matches(['*', '`', '.']).to_lowercase();
                    out.status = match word.as_str() {
                        "done" => Some(Status::Done),
                        "partial" => Some(Status::Partial),
                        "blocked" => Some(Status::Blocked),
                        _ => None,
                    };
                    None
                }
                "summary" => Some(&mut out.summary),
                "changes" => Some(&mut out.changes),
                "verification" => Some(&mut out.verification),
                "follow-ups" => Some(&mut out.follow_ups),
                _ => None,
            };
            if let Some(field) = current.as_deref_mut() {
                field.push_str(value);
            }
        } else if let Some(field) = current.as_deref_mut() {
            // A wrapped or nested line belongs to the previous key.
            if !line.is_empty() {
                field.push('\n');
                field.push_str(line);
            }
        }
    }
    out.status.map(|_| out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_last_block_with_wrapped_values() {
        let text = "Work log.\n\n## Result\n- status: blocked\n\nMore work.\n\n```\n## Result\n- status: Done\n- summary: Fixed it.\n- changes: a.rs\n  b.rs\n- verification: cargo test passed\n- follow-ups: none\n```\n";
        let o = parse(text).unwrap();
        assert_eq!(o.status, Some(Status::Done));
        assert_eq!(o.summary, "Fixed it.");
        assert_eq!(o.changes, "a.rs\nb.rs");
        assert_eq!(o.follow_ups, "none");
    }

    #[test]
    fn nested_bullets_stay_in_their_field() {
        let text = "## Result\n- status: partial\n- changes:\n  - a.rs: status: rewritten\n  - b.rs\n- extra: ignored\n- follow-ups: none\n";
        let o = parse(text).unwrap();
        assert_eq!(o.status, Some(Status::Partial));
        assert_eq!(o.changes, "\n- a.rs: status: rewritten\n- b.rs");
        assert_eq!(o.follow_ups, "none");
    }

    #[test]
    fn tolerates_emphasis_and_a_fence_after_the_heading() {
        for status in [
            "- **status:** done",
            "- **Status**: **Done**",
            "* status: `done`.",
            "  - status: done",
        ] {
            let text = format!("## Result\n\n```\n{status}\n- **summary:** ok\n```\ntrailing\n");
            let o = parse(&text).unwrap_or_else(|| panic!("{status}"));
            assert_eq!(o.status, Some(Status::Done), "{status}");
            assert_eq!(o.summary, "ok");
        }
        // Missing fields are empty, not an error; a block without a status is.
        assert_eq!(parse("## Result\n- status: blocked").unwrap().changes, "");
        assert!(parse("## Result\n- summary: no status\n").is_none());
    }

    #[test]
    fn missing_or_unknown_status_is_none() {
        assert!(parse("no block here").is_none());
        assert!(parse("## Result\n- status: done | partial | blocked\n").is_none());
    }
}
