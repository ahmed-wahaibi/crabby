# Crabby

**An AI agent orchestrator for Claude Code, written in Rust.** Crabby ships with 152 specialist agents (Rust, Go, Python, TypeScript, Kubernetes, security review, documentation and more) and runs the right one on your task from the terminal, with the model, reasoning effort and token cost under your control.

Crabby is at an early stage. The chat, running a single agent and planning which agents a task needs work today; running a multi-agent plan is being built. [FEATURES.md](FEATURES.md) marks every feature as built or planned.

## What works today

- **A chat in your terminal.** Run `crabby`: a transcript, an input box and a status line. A plain message is a conversation with one Claude Code session in the current folder; replies stream in, tool calls show as dim one-line entries, and follow-ups continue the same conversation.
- **Slash commands** for everything else: `/agents`, `/run`, `/plan`, `/model`, `/effort`, `/mode`, `/clear`, `/help`, `/quit`. Typing `/` lists them and Tab completes.
- **152 built-in agents** from [sc-agents](https://github.com/ahmed-wahaibi/sc-agents), compiled into one binary. Nothing to download at run time.
- **Run any agent on a task** with `/run`, as a headless Claude Code session limited to the tools that agent declares.
- **Plan with `/plan`:** a small model picks the agents for a task and shows the plan. Nothing is executed.
- **Model and effort** with `/model` and `/effort`, with a ceiling that asks you to type `yes` before running Fable above medium effort.
- **Token and cost summary:** a running total in the status line, and an exact line after every `/run` and `/plan`.
- **Script-friendly:** `crabby -p "<message or /command>"` runs one input without the interface, and the exit code reflects what an agent reported.

## Requirements

- [Claude Code](https://claude.com/claude-code) installed and signed in. Crabby runs agents through the `claude` command.
- Rust 1.85 or newer, to build from source.

## Install

```bash
cargo install --git https://github.com/ahmed-wahaibi/crabby
```

## Usage

Run `crabby` in the folder you want to work on. It needs a real terminal at least 40 columns wide and 10 rows high.

| Command | Does |
|---|---|
| `/help` | lists the commands |
| `/agents [text]` | lists agents, filtered by text in the name, category or description |
| `/run <agent> <task>` | runs one agent on a task, with only the tools in its header |
| `/plan <task>` | the router picks agents and shows the plan; nothing is executed |
| `/model [name]` | shows or sets the model for messages and `/run` |
| `/effort [level]` | shows or sets the effort (`low`, `medium`, `high`, `xhigh`, `max`, `default`) |
| `/mode [read\|edit]` | shows or sets what a plain conversation may do |
| `/clear` | starts a new conversation and clears the transcript |
| `/quit` | exits |

Keys: Enter sends, Alt+Enter adds a line, Up and Down recall earlier inputs, PageUp and PageDown scroll, Esc or Ctrl+C stops a reply, Ctrl+C twice on an empty prompt quits. A message sent while a reply is still streaming is not sent; stop the reply first.

**Modes.** A headless Claude Code session cannot stop to ask permission, so a plain conversation runs with a fixed set of tools. In `read` mode (the default) that is `Read`, `Grep` and `Glob`. In `edit` mode it also includes `Edit`, `Write` and `Bash`, so the conversation can change files and run commands without asking. The mode is shown in the status line. `/run` ignores the mode and uses the tools in the agent's header.

**Fable ceiling.** If the model is Fable, or Crabby cannot tell which model is set, `/effort high` (or `xhigh`, `max`) asks you to type `yes` in the input box. Anything else sets `medium`.

A plan lists one step per line with the agent, model, effort and what it depends on:

```
1  sc-backend-engineer          sonnet · medium  Add a rate limiter to the upload route …
2  sc-test-automation-engineer  sonnet · medium  after 1 · Write tests for the limiter …
```

Choosing the agents costs about 8,000 input tokens on Haiku, a fraction of a cent.

After a `/run` or `/plan`, Crabby shows one line with what it cost:

```
crabby: claude-haiku-5-5 · effort low · 1,204 in / 310 out · 18,400 cached · $0.0031 · 12s
```

### From scripts

`crabby -p` runs one input without the interface and prints the result on stdout. The input is a message or a slash command; `-p -` reads it from standard input. Tool activity and the cost line go to stderr. `--model`, `--effort` and `--mode` set the starting values (`/model`, `/effort`, `/mode`, `/clear` and `/quit` only work in the chat).

```bash
crabby -p "what does src/main.rs do?"
crabby -p "/plan add rate limiting to the upload endpoint and cover it with tests"
crabby -p "/run sc-code-reviewer review the changes in src/" --model sonnet --effort high
crabby -p - < task.txt
crabby -p "/agents rust" | cut -f1        # tab-separated when piped
```

Without `-p`, Crabby refuses to start unless both stdin and stdout are a terminal. In `-p` mode there is nobody to ask, so Fable above medium effort runs at `medium`.

### Exit codes (`-p`)

| Code | Meaning |
|---|---|
| 0 | done: a reply, a plan or a list; for `/run`, the agent reported `done` |
| 1 | `/run`: the agent reported `partial` |
| 2 | `/run`: the agent reported `blocked` |
| 3 | `/run`: the agent finished without a result block |
| 4 | Crabby could not do it (unknown agent, `claude` missing or failed) |
| 64 | usage error (bad flag, unknown command, or no terminal and no `-p`) |

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
crabby --agents-dir ../sc-agents/categories
```

## Roadmap

Planned, in order. Details are in [FEATURES.md](FEATURES.md).

1. **Plan approval:** accept, edit or reject the plan Crabby drafts before anything runs.
2. **Multi-agent execution:** independent steps in parallel, results passed between steps, a review stage after every change.
3. **More chat commands:** `/orchestrate`, `/review`, `/docs`, `/create-issue`, `/memory` and `/cost`, and the mascot's animation.
4. **Token savings:** effort picked per step, terse agent-to-agent hand-offs, and a token ledger.
5. **Global memory:** rules you save once apply to every project, without touching any repository.

## Contributing

Issues and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first; it covers the build, the checks a change must pass and the commit format. Agent definitions live in [sc-agents](https://github.com/ahmed-wahaibi/sc-agents), not in this repository.

## License

[MIT](LICENSE)
