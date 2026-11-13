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
    for line in &lines[start + 1..] {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with('#') {
            break;
        }
        if let Some((key, value)) = line.strip_prefix("- ").and_then(|l| l.split_once(':')) {
            let value = value.trim();
            current = match key.trim() {
                "status" => {
                    out.status = match value.to_lowercase().as_str() {
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
    fn missing_or_unknown_status_is_none() {
        assert!(parse("no block here").is_none());
        assert!(parse("## Result\n- status: done | partial | blocked\n").is_none());
    }
}
