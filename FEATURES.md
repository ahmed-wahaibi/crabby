# Crabby feature list

Crabby takes a task, picks the right SuperCrab agents for it, runs them through Claude Code, and reports what they did. Status: **built** means it is in `main` and tested; **planned** means designed but not written.

## 1. Agent catalogue

| Feature | Status |
|---|---|
| All agents from `sc-agents` compiled into the binary (pinned by submodule commit) | built |
| Every agent validated at load: header fields, name matches file, result section present | built |
| Invalid, unreadable or duplicate agents skipped with a warning, never a crash | built |
| `/agents`, grouped by category, fitted to the window width | built |
| `/agents <text>`: only agents with the text in the name, category or description | built |
| Tab-separated output when piped (`crabby -p "/agents" \| cut -f1`), for `grep`, `cut` and scripts | built |
| `--agents-dir <folder>` to use a working copy instead of the built-in set | built |
| `/show <agent>`: full description, tools, default model and effort | planned |

## 2. Running one agent

| Feature | Status |
|---|---|
| `/run <agent> <task>` in the current folder, as a headless Claude Code process, streaming into the chat | built |
| From scripts: `crabby -p "/run <agent> <task>"`, or the whole input from standard input with `crabby -p -` | built |
| Only the tools named in the agent's header are permitted | built |
| Live output: agent text and one-line tool entries in the transcript (with `-p`: text on stdout, tool activity on stderr) | built |
| Result block parsed into status, summary, changes, verification, follow-ups | built |
| Result block parsed again into a status line after the run, with the usage line | built |
| Exit code from the status with `-p`: 0 done, 1 partial, 2 blocked, 3 no result block, 4 could not run, 64 usage error | built |
| "Did you mean" suggestions for a mistyped agent name | built |
| Works with Claude Code installed as `claude.exe` or `claude.cmd` | built |
| Agent files of any size (instructions passed by file, not command line) | built |

## 3. Model and effort per agent run

Each agent run gets its own model and its own effort level. Neither is fixed in the agent file, so the same agent can run cheaply on a small job and at full strength on a hard one.

| Feature | Status |
|---|---|
| `/model [name]` and `/effort [level]` in the chat; `--model` and `--effort` as starting values and with `-p` | built |
| Model accepted as an alias (`fable`, `opus`, `sonnet`, `haiku`) or an exact version (`claude-fable-5-2`, `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`) | planned |
| Aliases resolved to the exact model version at run time, and the exact version recorded with the run | planned |
| Router chooses the model for every step of a plan, from the difficulty and risk of that step; the effort judge below picks the effort | planned |
| Defaults by kind of work, set in config (see below) | planned |
| Per-agent override in config, e.g. always run `sc-defect-reviewer` at high effort | planned |
| Model and effort shown beside each step in the plan, editable before approval | planned |
| Fallback model per run, used when the chosen model is overloaded or unavailable | planned |
| Automatic step-up: a step that comes back `blocked` or `partial` can be retried once on a stronger model or higher effort | planned |
| Model version, effort, tokens and cost stored for every run in the run log | planned |
| **Fable effort ceiling:** a run on Fable is capped at `medium` effort. Anything higher, whether you asked for it, the judge picked it or a retry stepped it up, stops and asks you to type `yes` first (in the chat, in the input box) | built for `/effort` and `/model`; the judge and retries are planned |
| Any other answer, or no terminal to ask on (`-p`, scripts, CI, `--yes`), runs the step at `medium` and says so | built |
| The ceiling applies per step, so approving one step does not approve the next | planned |

### Effortless: effort picked per run

[effortless](https://github.com/HeyCubit/effortless) is a third-party, MIT-licensed Claude Code mod that has a small, fast model judge each prompt and pick the reasoning effort for it. It is built for the interactive Claude Code window, so Crabby applies the same approach natively to each agent run.

| Feature | Status |
|---|---|
| **Effort judge:** before each agent run, a quick Haiku call reads that step's instruction and picks `low`, `medium`, `high`, `xhigh` or `max` | planned |
| The judge is on by default (`--effort auto`); an effort you set yourself always wins | planned |
| The judge's pick and its one-line reason shown beside the step, in the plan and the TUI | planned |
| Lean setting: `cheaper` or `smarter`, shifting the judge's picks down or up one level | planned |
| Per-agent floor and ceiling in config, e.g. reviewers never below `high` | planned |
| Save mode: effort capped at `medium` once the 5-hour or weekly usage limit passes 80% | planned |
| Cheaper model when it pays: a step the judge calls simple may run on Haiku or Sonnet, never above the model set for it (off by default) | planned |
| Status bar: prompt-cache countdown and context fullness for the selected agent | planned |
| If the judge fails or is slow, the default effort for that kind of work is used | planned |
| Credit and licence notice for effortless shipped with Crabby | planned |

Starting defaults (changeable in config). With the effort judge on, the effort column is the fallback and the model column still applies:

| Kind of work | Example agents | Model | Effort |
|---|---|---|---|
| Routing and planning | the router itself | sonnet | medium |
| Lookups, formatting, simple edits | `sc-readme-writer`, `sc-docs-drift-editor` | haiku | low |
| Everyday engineering | `sc-rust-engineer`, `sc-backend-engineer` | sonnet | medium |
| Review, audit, debugging | `sc-defect-reviewer`, `sc-security-auditor`, `sc-debugger` | opus | high |
| Architecture and hard design | `sc-cloud-architect`, `sc-llm-architect` | fable | medium |
| Explicitly requested deep work | any agent, on request | fable | above medium only after you type `yes` |

## 4. Automatic agent selection

| Feature | Status |
|---|---|
| `/plan <task>`: the router picks one or several agents and shows a plan; also `crabby -p "/plan <task>"` | built |
| Router reads the catalogue by name and description, with no hard-coded agent list | built |
| Plan output: ordered steps, the agent for each, its instruction, and which steps depend on which | built |
| Steps the router cannot match to any agent are marked unassigned, not forced onto a poor fit | built |
| Plan approval before anything runs: accept, drop a step, swap an agent, change model or effort, or retype the task | planned |
| `--yes` to skip approval for unattended runs | planned |
| Saved pipelines: a named sequence in a file, run the same way every time | planned |

## 5. Multi-agent execution

| Feature | Status |
|---|---|
| Independent steps run in parallel, dependent steps in order | planned |
| A step's result block is passed to the steps that depend on it | planned |
| Limit on how many agents run at once | planned |
| Blocked step: stop its dependents, keep unrelated steps going, offer retry or reassignment | planned |
| One writer per file at a time, so parallel agents cannot overwrite each other | planned |
| Final summary: agents run, files changed, what was verified, follow-ups raised | planned |
| Overall exit code for scripts and CI: worst status across all steps | planned |

## 6. Ponytail (lean-code mode and review stage)

[Ponytail](https://github.com/DietrichGebert/ponytail) is a third-party, MIT-licensed "lazy senior developer" mode: the smallest change that fully solves the task. Crabby uses it in two places.

| Feature | Status |
|---|---|
| **Review stage after every agent run that changed files:** a read-only Ponytail review of that agent's diff (is the logic right, is it safe, is risky code tested, is every line needed) | planned |
| Review findings go back to the same agent for one fix round; the step is then re-reviewed once | planned |
| A step is only marked `done` when its review passes; otherwise it is `partial` with the open findings listed | planned |
| The review is its own entry in the plan and the TUI, with its own model and effort (default: opus, high) | planned |
| Agents that changed no files (research, analysis, advice) skip the review stage | planned |
| **Working mode:** Ponytail's rules added to every engineering agent's instructions, at level `lite`, `full` (default) or `ultra` | planned |
| `--no-review` and `--ponytail <off\|lite\|full\|ultra>` per run, and the same settings in config | planned |
| Ponytail's text pinned by submodule and compiled in, with its licence and copyright notice shipped alongside | planned |
| End-of-run "shortcuts taken" list, collected from the `shortcut:` comments agents leave | planned |

## 7. Tokenomics (Caveman)

[Caveman](https://github.com/juliusbrussee/caveman) is a third-party, Apache-2.0 project that cuts token use: agents say less and read less, while code, commands, paths, numbers and error messages stay exact. Most of what Crabby's agents write is read by another agent or by Crabby, not by you, which is where terse output costs nothing.

| Feature | Status |
|---|---|
| **Say less:** Caveman's voice rules added to every agent's instructions for working output and agent-to-agent hand-offs | planned |
| Levels `off`, `caveman` (default) and `ultracave`, per run (`--caveman <level>`) and in config | planned |
| The final summary shown to you is always written in full sentences, whatever the level | planned |
| Security warnings and anything irreversible are always written in full sentences | planned |
| Your own task text is never rewritten | planned |
| **Read less:** long tool output (test runs, logs, JSON, CSV) shrunk before the agent reads it | planned |
| Hand-offs between steps carry the result block and the files changed, not the whole transcript | planned |
| Prompt cache kept warm between steps of the same plan, so later steps re-read less at full price | planned |
| **Token ledger:** tokens in, tokens out, cache hits and cost per agent run, per step and per plan | built per run and per routing call; per plan planned |
| Savings shown against an estimate of the same run without Caveman | planned |
| `crabby stats`: where tokens went across past runs, by agent and by model | planned |
| If the Caveman CLI is installed, Crabby detects it and does not apply the voice rules twice | planned |
| Caveman's rules pinned by submodule and compiled in, with its licence and notice shipped alongside | planned |

Caveman's own "Auto" feature also picks model and effort. Crabby leaves that to the router and the effort judge in section 3, so the two do not compete.

## 8. Terminal interface and slash commands

Running `crabby` with no arguments opens a chat that works the way Claude Code's does: a prompt at the bottom, replies streaming above it, tool activity shown inline, and slash commands. A plain message is an ordinary single-assistant conversation. Orchestration is something you ask for with a command, not the default. The look, name and mascot are Crabby's own.

| Feature | Status |
|---|---|
| Chat interface: transcript, input box, status line; streaming replies, inline tool activity, multi-line input (Alt+Enter), history with the arrow keys | built |
| A plain message goes to one Claude Code session in the current project, with no router and no extra agents | built |
| Conversation kept across messages (`--resume`) | built |
| Conversation resumable after restart | planned |
| `/mode read\|edit`: what a plain conversation may do, since a headless session cannot ask. `read` pre-approves read and search tools, `edit` also file edits and shell commands | built |
| `/` opens a command list with completion (Tab); `/help` describes each command | built |
| **`/orchestrate <task>`**: the router picks agents and drafts a plan, you approve it, and the agents run (sections 4 and 5) | planned |
| **`/create-issue <description>`**: drafts a GitHub issue for the current project (title, body, labels, taken from your description and the repo), shows it to you, and creates it once you approve | planned |
| **`/docs [what]`**: brings the project's documentation in line with the code, for everything or only what you name, and shows the changes before they are kept | planned |
| **`/review <PR links…>`**: reviews the pull requests whose links you append | planned |
| **`/review all`**: reviews every open pull request in the current project | planned |
| **`/review <description>`**: you describe the pull request in words and Crabby finds the matching one, asking when more than one fits | planned |
| Reviews list findings by severity with file and line; posting them to the pull request as a comment happens only when you approve | planned |
| `/agents` to browse the catalogue, `/run <agent> <task>` for one agent | built |
| `/model`, `/effort` and `/clear` | built |
| `/memory` and `/cost` | planned |
| Where it makes sense, a command also works from the shell with `crabby -p "/command"`, for scripts and CI | built for the commands above, planned for the rest |
| During `/orchestrate`: a plan panel with a status per step beside the live output of the selected agent | planned |
| Switch between running agents with the arrow keys | planned |
| Status line: model, effort, mode, tokens, cost so far, current folder | built |
| Status line: agents running | planned |
| Model and effort shown per step | planned |
| Search within an agent's output | planned |
| Stop a reply or a `/run` from the keyboard (Esc or Ctrl+C); the `claude` process is killed | built |
| Stop one agent of several | planned |
| Plain non-interactive output with `crabby -p`; without `-p` and without a terminal Crabby exits with a usage error | built |
| Colour that respects `NO_COLOR` | built |
| Scrolling with PageUp and PageDown | built |

## 9. Mascot

| Feature | Status |
|---|---|
| Pixel-art crab on the welcome screen, drawn in coloured half-block characters from `assets/crab.txt`; shown only in a window of at least 24x14, with colour allowed (`NO_COLOR` unset) and a true-colour terminal (`COLORTERM=truecolor`), otherwise just the name | built |
| The cape and chest shield | planned |
| Animation states: idle, typing while agents run, flying on success, slumped on a blocked step | planned |
| Option to turn the mascot off | planned |

## 10. Cost and safety

| Feature | Status |
|---|---|
| Spend cap per run and per plan; the run stops when it is reached | planned |
| Estimated cost shown with the plan before approval | planned |
| Agents never get tools beyond their header, whatever the task says | built |
| Confirmation prompt before a plan that includes agents with shell access runs in a folder for the first time | planned |
| Temporary files cleaned up after every run | built (not on Ctrl-C) |

## 11. Global memory (never committed)

Rules you tell Crabby to remember are kept in one store on your machine, outside every project, and applied to all work Crabby does in any project. Saying "save to memory the rule of no co-author" while working in one repo makes that rule apply in every future project, and nothing is written to that repo.

| Feature | Status |
|---|---|
| One global store in your user folder (`~/.crabby/memory/`), outside every repository, so it can never be committed or pushed | planned |
| Saving in plain words, from the TUI or the command line: "save to memory …", or `crabby remember "<rule>"` | planned |
| A saved rule applies to every agent run in every project from then on | planned |
| Nothing is added to the current project's `CLAUDE.md`, and no file inside the project is created or changed | planned |
| The rules are passed to each agent with its instructions at run time, not written into the agent files | planned |
| `crabby memory` to list the rules, `crabby forget <id>` to remove one, and plain-text files you can edit by hand | planned |
| Order of authority: what you say in the current task, then a memory rule, then the agent's own defaults | planned |
| A new rule that contradicts an existing one is flagged, and you choose which stays | planned |
| Each rule stored as one short line with the date and the project it was saved from | planned |
| Token cost kept small: rules are sent as a compact list, with a size cap and a warning when the store grows past it | planned |
| Optional project-only rule (`--project`), still stored outside the repo and keyed by the project's path | planned |
| `--no-memory` to run once without any saved rules | planned |

## 12. History and configuration

| Feature | Status |
|---|---|
| Run log per session: task, plan, each agent's model version, effort, result block, tokens, cost, duration | planned |
| `crabby history` and `crabby resume <id>` to continue a stopped plan from its last finished step | planned |
| Config file for defaults: model and effort by kind of work, per-agent overrides, parallel limit, spend cap, mascot | planned |
| Project-level config that overrides the user-level one | planned |

## 13. Distribution

| Feature | Status |
|---|---|
| Single binary with the agents inside; works offline apart from the Claude calls | built |
| Minimum Rust version declared (1.85) | built |
| Prebuilt binaries for Windows, macOS and Linux on each release | planned |
| New agents arrive by moving the `sc-agents` submodule and cutting a release | built (manual) |
