# Contributing to Crabby

Thanks for helping. This page covers how to build Crabby, what a change must pass, and how to write commits.

## Before you start

- **Bugs and ideas:** open an issue using one of the templates. For anything larger than a small fix, open the issue first so the approach can be agreed before you write code.
- **Agent definitions** (the `sc-*.md` files) live in [sc-agents](https://github.com/ahmed-wahaibi/sc-agents). Changes to an agent's instructions go there. This repository only pins a commit of it in `agents/`.
- **Scope:** [FEATURES.md](FEATURES.md) lists what is built and what is planned. A pull request should match something listed there, or come with an issue explaining why it should be added.

## Build and test

Crabby needs Rust 1.85 or newer. Clone with the agents submodule:

```bash
git clone --recurse-submodules https://github.com/ahmed-wahaibi/crabby
cd crabby
cargo build
```

Every change must pass these three before it is reviewed:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Running anything for real (a message or `/run` in the chat, or `crabby -p …`) needs [Claude Code](https://claude.com/claude-code) installed and signed in, and spends tokens on your account. The unit tests do not. The chat itself needs a real terminal; its logic and drawing are tested without one (a pure `update` function in `app` and drawing to ratatui's test backend in `ui`). Try the screen by hand before changing it.

## Rules for a change

1. **Keep it small.** One pull request does one thing. Prefer the smallest change that fully solves the problem, and reuse what is already in the codebase or the standard library before adding a dependency.
2. **Test new logic.** A new branch, parser or decision gets a unit test. Keep decisions in pure functions so they can be tested without starting `claude`.
3. **Do not change behaviour silently.** Exit codes, the format of piped `/agents` output and the result block are relied on by scripts. A change to any of them must be called out in the pull request.
4. **No terminal or printing code in the core modules.** `catalog`, `outcome`, `runner` and `router` return data; `main` decides how to show it. `app` holds the chat state and a pure `update`, `ui` only draws, `commands` only parses.
5. **Count the tokens.** Crabby exists partly to keep token use down. If a change adds a model call or makes a prompt longer, say what it costs in the pull request.
6. **Never commit secrets,** API keys or anything from your Claude Code configuration.

## Commit rules

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <summary in the imperative, lower case, no full stop>

<optional body: what changed and why, wrapped at about 72 characters>
```

| Type | Use for |
|---|---|
| `feat` | a new capability for users |
| `fix` | a bug fix |
| `docs` | documentation only |
| `test` | adding or correcting tests |
| `refactor` | a code change that neither fixes a bug nor adds a feature |
| `perf` | a change that makes something faster or cheaper in tokens |
| `build` | the build, dependencies or minimum Rust version |
| `ci` | continuous integration |
| `chore` | anything else that does not touch source or tests |

Scopes in use: `cli`, `run`, `runner`, `router`, `catalog`, `outcome`, `github`, `chat`.

Examples:

```
feat(run): add --effort and cap Fable at medium effort
fix(runner): avoid a deadlock on large tasks
docs: add feature list
build: declare minimum Rust version 1.85
```

Further rules:

- **One logical change per commit,** so each commit has one accurate type.
- **Breaking changes** get a `!` after the type or scope (`feat(cli)!: …`) and a `BREAKING CHANGE:` line in the body saying what breaks.
- **Do not rewrite history** on `main`.
- **Updating the agents:** move the `agents` submodule to the new `sc-agents` commit in its own commit, typed `chore(agents): …`, and run `cargo test`, which checks that every bundled agent is valid.

## Pull requests

- Describe what changed, why, and how you checked it.
- Link the issue it closes (`Closes #12`).
- Keep the branch up to date with `main` by rebasing, not by merging `main` into it.

## License

By contributing you agree that your contribution is licensed under the [MIT License](LICENSE).
