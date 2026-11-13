//! Model, effort and mode settings, and the Fable effort ceiling. No printing here.

use clap::ValueEnum;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn name(self) -> String {
        self.to_possible_value()
            .expect("no skipped variants")
            .get_name()
            .to_string()
    }
}

#[derive(Debug, PartialEq)]
pub enum Gate {
    Proceed,
    Ask,
    CapToMedium,
    /// Possibly Fable with no `--effort`: pass `medium` instead of Claude Code's default.
    DefaultMedium,
}

/// Decides whether a run may start at the requested effort. `model` is `None` when
/// it is unknown, which is treated as possibly Fable.
pub fn gate(model: Option<&str>, effort: Option<Effort>, terminal: bool) -> Gate {
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

/// What a plain conversation may do. A headless session cannot ask, so each mode
/// pre-approves a fixed set of tools.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Mode {
    Read,
    Edit,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Read => "read",
            Mode::Edit => "edit",
        }
    }

    pub fn tools(self) -> &'static str {
        match self {
            Mode::Read => "Read,Grep,Glob",
            Mode::Edit => "Read,Grep,Glob,Edit,Write,Bash",
        }
    }
}

/// The effort a run really gets. Fable above `medium` only keeps its effort when
/// `confirmed` (the user typed `yes`); an unknown or Fable model with no effort gets
/// `medium` instead of Claude Code's default.
pub fn effective_effort(
    model: Option<&str>,
    effort: Option<Effort>,
    confirmed: bool,
) -> Option<Effort> {
    match gate(model, effort, true) {
        Gate::Ask if confirmed => effort,
        Gate::Ask | Gate::CapToMedium | Gate::DefaultMedium => Some(Effort::Medium),
        Gate::Proceed => effort,
    }
}

/// Checks a model name: an alias or a `claude-*` id.
pub fn check_model(m: &str) -> Result<(), String> {
    let m = m.to_ascii_lowercase();
    let ok = ["fable", "opus", "sonnet", "haiku"].contains(&m.as_str())
        || (m.starts_with("claude-") && !m.contains(char::is_whitespace));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "unknown model `{m}`; use fable, opus, sonnet, haiku or an id such as claude-sonnet-5-5"
        ))
    }
}

/// The model Claude Code would use when `--model` is not given: ANTHROPIC_MODEL, else
/// the first `model` in the local, project or user settings.
// shortcut: managed (enterprise) settings and an `env` block in settings are not read,
// so a model set only there is missed; `None` then makes the caller assume Fable.
pub fn configured_model() -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_effort_never_lets_fable_exceed_medium_unconfirmed() {
        use Effort::*;
        let e = effective_effort;
        assert_eq!(e(Some("fable"), Some(High), false), Some(Medium));
        assert_eq!(e(Some("fable"), Some(High), true), Some(High));
        assert_eq!(e(None, Some(Max), false), Some(Medium));
        assert_eq!(e(Some("fable"), None, true), Some(Medium));
        assert_eq!(e(Some("opus"), Some(Max), false), Some(Max));
        assert_eq!(e(Some("opus"), None, false), None);
    }

    #[test]
    fn models_are_checked() {
        for ok in ["fable", "Haiku", "claude-sonnet-5-5"] {
            assert!(check_model(ok).is_ok(), "{ok}");
        }
        for bad in ["", "gpt-4", "claude-x y", "my fable"] {
            assert!(check_model(bad).is_err(), "{bad}");
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
}
