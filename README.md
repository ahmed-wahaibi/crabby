# Crabby

**An AI agent orchestrator for Claude Code, written in Rust.** Crabby ships with 152 specialist agents (Rust, Go, Python, TypeScript, Kubernetes, security review, documentation and more) and runs the right one on your task from the terminal, with the model, reasoning effort and token cost under your control.

Crabby is at an early stage. Running a single agent works today; automatic agent selection, multi-agent plans and the chat interface are being built. [FEATURES.md](FEATURES.md) marks every feature as built or planned.

## What works today

- **152 built-in agents** from [sc-agents](https://github.com/ahmed-wahaibi/sc-agents), compiled into one binary. Nothing to download at run time.
- **Run any agent on a task** in the current folder, as a headless Claude Code session limited to the tools that agent declares.
- **Live output:** the agent's text streams to stdout and its tool calls to stderr.
- **Model and effort per run:** `--model` and `--effort`, with a ceiling that asks before running Fable above medium effort.
- **Token and cost summary** after every run: exact model, tokens in and out, cached tokens, cost and duration.
- **Script-friendly:** the exit code reflects what the agent reported, and `list` output is tab-separated when piped.

## Requirements

- [Claude Code](https://claude.com/claude-code) installed and signed in. Crabby runs agents through the `claude` command.
- Rust 1.85 or newer, to build from source.

## Install

```bash
cargo install --git https://github.com/ahmed-wahaibi/crabby
```

## Usage

```bash
# See the agents
crabby list
crabby list --category language-specialists
crabby list | grep -i rust

# Run one agent on a task in the current folder
crabby run sc-rust-engineer "make the parser report line numbers"

# Choose the model and reasoning effort
crabby run sc-code-reviewer --model sonnet --effort high "review the changes in src/"

# Read the task from a file
crabby run sc-docs-engineer - < task.md
```

After a run, Crabby prints one line with what it cost:

```
crabby: claude-haiku-5-5 · effort low · 1,204 in / 310 out · 18,400 cached · $0.0031 · 12s
```

### Exit codes

| Code | Meaning |
|---|---|
| 0 | the agent reported `done` |
| 1 | the agent reported `partial` |
| 2 | the agent reported `blocked` |
| 3 | the agent finished without a result block |
| 4 | Crabby could not run it (unknown agent, `claude` missing or failed) |
| 64 | usage error |

## How it works

Each agent is a Markdown file with a short header (name, description, category, tools) and its instructions. Crabby passes the instructions to a headless `claude` process, permits only the tools in the header, and reads the result block the agent ends with:

```
## Result
- status: done | partial | blocked
- summary: one or two sentences
- changes: files created or modified, or "none"
- verification: what was run or checked, and the outcome
- follow-ups: out-of-scope work, open risks, or "none"
```

To try agents from a working copy instead of the built-in set, point Crabby at a `categories` folder:

```bash
crabby --agents-dir ../sc-agents/categories list
```

## Roadmap

Planned, in order. Details are in [FEATURES.md](FEATURES.md).

1. **Router:** describe a task and Crabby picks one or several agents and drafts a plan for you to approve.
2. **Multi-agent execution:** independent steps in parallel, results passed between steps, a review stage after every change.
3. **Chat interface:** a terminal chat with slash commands such as `/orchestrate`, `/review`, `/docs` and `/create-issue`.
4. **Token savings:** effort picked per step, terse agent-to-agent hand-offs, and a token ledger.
5. **Global memory:** rules you save once apply to every project, without touching any repository.

## Contributing

Issues and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first; it covers the build, the checks a change must pass and the commit format. Agent definitions live in [sc-agents](https://github.com/ahmed-wahaibi/sc-agents), not in this repository.

## License

[MIT](LICENSE)
