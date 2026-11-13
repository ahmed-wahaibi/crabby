//! The router: asks a small model to split a task into steps, one agent each.
//! Returns data only; `main` prints it.

use crate::catalog::Agent;
use crate::runner::{self, Usage};
use serde_json::Value;
use std::{collections::HashSet, io, sync::atomic::AtomicBool};

const MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

const SYSTEM: &str = r#"You route a task to specialist agents from the catalogue. Reply with JSON only: {"steps":[{"id":1,"agent":"<catalogue name>","task":"<instruction>","after":[<ids of earlier steps>],"model":"haiku|sonnet|opus|fable","effort":"low|medium|high|xhigh|max"}],"unassigned":["<part no agent fits>"]}
Use as few steps as the task needs: one agent if one fits. "after" lists only earlier ids. A part that fits no agent goes in "unassigned", never to a poor fit. Pick the cheapest model and effort that suit each step; fable only for the hardest design or risk. Write instructions in the task's language."#;

#[derive(Debug, PartialEq)]
pub struct Step {
    pub id: u64,
    pub agent: String,
    pub task: String,
    pub after: Vec<u64>,
    pub model: String,
    pub effort: String,
}

#[derive(Debug, PartialEq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub unassigned: Vec<String>,
}

/// Fable by alias or exact id, as `gate` in `main` sees it.
fn is_fable(model: &str) -> bool {
    model == "fable" || model.starts_with("claude-fable")
}

/// Longest description sent per agent. The full catalogue at the first sentence alone
/// was 12k tokens; this keeps it under the 8k budget.
const MAX_DESC: usize = 110;

/// `name: description` per agent under its category, in catalogue order, each
/// description cut to its first sentence and then to `MAX_DESC` characters at a word.
// shortcut: the sentence cut is at the first ". ", so "e.g. " ends one early; fine for
// routing, fix if a description loses its point.
fn catalogue(agents: &[Agent]) -> String {
    let mut out = String::new();
    let mut cat = "";
    for a in agents {
        if a.category != cat {
            cat = &a.category;
            out += &format!("## {cat}\n");
        }
        let d = a.description.trim();
        let first = d.find(". ").map_or(d, |i| &d[..=i]);
        let first = match first.char_indices().nth(MAX_DESC) {
            Some((i, _)) => first[..i].rsplit_once(' ').map_or(&first[..i], |(h, _)| h),
            None => first,
        };
        out += &format!("{}: {first}\n", a.name);
    }
    out
}

/// Catalogue first, so repeat calls share a cacheable prefix; the task last.
fn prompt(agents: &[Agent], task: &str) -> String {
    format!("Catalogue:\n{}\nTask:\n{task}", catalogue(agents))
}

/// Takes the outermost `{...}` so a code fence or text around the JSON is ignored.
fn extract(answer: &str) -> Option<&str> {
    Some(&answer[answer.find('{')?..=answer.rfind('}')?])
}

/// Parses and checks a model answer against the catalogue. Fable above `medium`
/// is lowered to `medium`; running it still goes through `gate`.
pub fn parse_plan(answer: &str, agents: &[Agent]) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(extract(answer).ok_or("no JSON object in the answer")?)
        .map_err(|e| format!("invalid JSON: {e}"))?;
    fn text<'a>(s: &'a Value, k: &str) -> Option<&'a str> {
        s[k].as_str().map(str::trim).filter(|t| !t.is_empty())
    }
    let mut steps: Vec<Step> = Vec::new();
    let mut seen = HashSet::new();
    for (i, s) in v["steps"].as_array().into_iter().flatten().enumerate() {
        let n = i + 1;
        let id = s["id"]
            .as_u64()
            .ok_or(format!("step {n}: `id` must be a whole number"))?;
        if !seen.insert(id) {
            return Err(format!("step {n}: id {id} is used twice"));
        }
        let agent = text(s, "agent").ok_or(format!("step {id}: `agent` is missing"))?;
        if !agents.iter().any(|a| a.name == agent) {
            return Err(format!("step {id}: `{agent}` is not in the catalogue"));
        }
        let task = text(s, "task").ok_or(format!("step {id}: `task` is missing"))?;
        let mut after = Vec::new();
        for d in s["after"].as_array().into_iter().flatten() {
            let d = d
                .as_u64()
                .ok_or(format!("step {id}: `after` holds only ids"))?;
            if !steps.iter().any(|p| p.id == d) {
                return Err(format!("step {id}: `after` {d} is not an earlier step"));
            }
            after.push(d);
        }
        let model = text(s, "model").unwrap_or("").to_ascii_lowercase();
        let alias = MODELS.contains(&model.as_str());
        let exact = MODELS
            .iter()
            .any(|m| model.starts_with(&format!("claude-{m}-")));
        if !(alias || exact) || model.contains(char::is_whitespace) {
            return Err(format!("step {id}: unknown model `{model}`"));
        }
        let mut effort = text(s, "effort").unwrap_or("").to_ascii_lowercase();
        let rank = EFFORTS
            .iter()
            .position(|e| *e == effort)
            .ok_or(format!("step {id}: unknown effort `{effort}`"))?;
        if is_fable(&model) && rank > 1 {
            effort = "medium".into();
        }
        steps.push(Step {
            id,
            agent: agent.into(),
            task: task.into(),
            after,
            model,
            effort,
        });
    }
    let unassigned: Vec<String> = v["unassigned"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str().map(str::trim).filter(|u| !u.is_empty()))
        .map(String::from)
        .collect();
    if steps.is_empty() && unassigned.is_empty() {
        return Err("the plan has no steps and nothing unassigned".into());
    }
    Ok(Plan { steps, unassigned })
}

/// Asks the router for a plan, retrying once with the validation error.
/// The usage is the sum over both calls.
pub fn plan(
    agents: &[Agent],
    task: &str,
    model: &str,
    effort: &str,
    cancel: &AtomicBool,
) -> io::Result<(Plan, Usage)> {
    let first = prompt(agents, task);
    let mut total = Usage::default();
    let mut ask = |p: &str| -> io::Result<String> {
        let (answer, usage) = runner::ask(SYSTEM, p, Some(model), Some(effort), cancel)?;
        if let Some(u) = usage {
            total.input_tokens += u.input_tokens;
            total.output_tokens += u.output_tokens;
            total.cache_read_tokens += u.cache_read_tokens;
            total.cache_write_tokens += u.cache_write_tokens;
            total.cost_usd = Some(total.cost_usd.unwrap_or(0.0) + u.cost_usd.unwrap_or(0.0));
            total.duration_ms = Some(total.duration_ms.unwrap_or(0) + u.duration_ms.unwrap_or(0));
            total.models = u.models;
        }
        Ok(answer)
    };
    let answer = ask(&first)?;
    let plan = match parse_plan(&answer, agents) {
        Ok(p) => p,
        Err(e) => {
            let retry = format!(
                "{first}\n\nYour previous answer:\n{answer}\nIt was rejected: {e}\nReply with corrected JSON only."
            );
            let answer = ask(&retry)?;
            parse_plan(&answer, agents).map_err(|e| {
                io::Error::other(format!(
                    "the router's plan was invalid twice; last error: {e}"
                ))
            })?
        }
    };
    Ok((plan, total))
}

/// One line per step, then one per unassigned part.
pub fn render(plan: &Plan) -> String {
    let pad = plan.steps.iter().map(|s| s.agent.len()).max().unwrap_or(0);
    let mut out = String::new();
    for s in &plan.steps {
        let after = if s.after.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = s.after.iter().map(u64::to_string).collect();
            format!("after {} · ", ids.join(", "))
        };
        let task = s.task.replace('\n', " ");
        out += &format!(
            "{}  {:<pad$}  {} · {}  {after}{task}\n",
            s.id, s.agent, s.model, s.effort
        );
    }
    for u in &plan.unassigned {
        out += &format!("unassigned: {}\n", u.replace('\n', " "));
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

    fn cat() -> Vec<Agent> {
        vec![
            agent("sc-a", "one", "Builds things. Also other stuff."),
            agent("sc-b", "one", "Reviews"),
            agent("sc-c", "two", "Tests it."),
        ]
    }

    fn step(id: u64, agent: &str, after: &str, model: &str, effort: &str) -> String {
        format!(
            r#"{{"id":{id},"agent":"{agent}","task":"do {id}","after":[{after}],"model":"{model}","effort":"{effort}"}}"#
        )
    }

    fn err(steps: &[String]) -> String {
        let answer = format!(r#"{{"steps":[{}],"unassigned":[]}}"#, steps.join(","));
        parse_plan(&answer, &cat()).unwrap_err()
    }

    #[test]
    fn catalogue_groups_by_category_and_keeps_the_first_sentence() {
        assert_eq!(
            catalogue(&cat()),
            "## one\nsc-a: Builds things.\nsc-b: Reviews\n## two\nsc-c: Tests it.\n"
        );
        let long = [agent("sc-x", "c", &"word ".repeat(40))];
        assert_eq!(
            catalogue(&long).len(),
            "## c
sc-x: "
                .len()
                + 109
                + 1
        ); // 22 whole words
        assert!(prompt(&cat(), "the task").ends_with("\nTask:\nthe task"));
    }

    #[test]
    fn json_is_found_in_fences_and_padding_and_fable_is_lowered() {
        let body = format!(
            r#"{{"steps":[{},{}],"unassigned":["paint it"]}}"#,
            step(1, "sc-a", "", "fable", "max"),
            step(2, "sc-b", "1", "claude-opus-5-5", "high"),
        );
        for answer in [
            body.clone(),
            format!("```json\n{body}\n```"),
            format!("Here you go: {body} Done."),
        ] {
            let p = parse_plan(&answer, &cat()).unwrap();
            assert_eq!(p.steps[0].effort, "medium"); // Fable lowered
            assert_eq!(p.steps[1].effort, "high"); // others untouched
            assert_eq!(p.steps[1].after, [1]);
            assert_eq!(p.unassigned, ["paint it"]);
        }
        assert!(parse_plan("no json here", &cat()).is_err());
        assert!(parse_plan("{not json}", &cat()).is_err());
    }

    #[test]
    fn invalid_plans_are_rejected() {
        let ok = |id, after: &str| step(id, "sc-a", after, "haiku", "low");
        assert!(err(&[step(1, "sc-zzz", "", "haiku", "low")]).contains("not in the catalogue"));
        assert!(err(&[ok(1, ""), ok(1, "")]).contains("twice"));
        assert!(err(&[ok(1, "2"), ok(2, "")]).contains("not an earlier step")); // forward
        assert!(err(&[ok(1, "1")]).contains("not an earlier step")); // cycle
        assert!(err(&[step(1, "sc-a", "", "gpt", "low")]).contains("unknown model"));
        assert!(err(&[step(1, "sc-a", "", "haiku", "huge")]).contains("unknown effort"));
        assert!(err(&[]).contains("nothing unassigned"));
        // Only unassigned parts is a valid plan.
        let p = parse_plan(r#"{"steps":[],"unassigned":["x"]}"#, &cat()).unwrap();
        assert!(p.steps.is_empty());
    }

    #[test]
    fn plan_renders_for_the_terminal() {
        let answer = format!(
            r#"{{"steps":[{},{}],"unassigned":["paint it"]}}"#,
            step(1, "sc-a", "", "sonnet", "medium"),
            step(2, "sc-c", "1", "opus", "high"),
        );
        let p = parse_plan(&answer, &cat()).unwrap();
        assert_eq!(
            render(&p),
            "1  sc-a  sonnet · medium  do 1\n2  sc-c  opus · high  after 1 · do 2\nunassigned: paint it\n"
        );
    }
}
