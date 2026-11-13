//! Text shown to the user: token summary and the agent list.

use crate::catalog::Agent;
use crate::runner;
use crate::settings::Effort;

/// `1234567` -> `1,234,567`.
pub fn commas(n: u64) -> String {
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
pub fn summary(u: &runner::Usage, effort: Option<Effort>) -> String {
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

/// Renders the agent list. `width` is the terminal width; `None` means stdout is
/// not a terminal and gets one tab-separated line per agent.
pub fn render_list(agents: &[&Agent], width: Option<usize>) -> String {
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
