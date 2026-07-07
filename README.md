# Inductor

Inductor is a terminal-native AI coding agent. You point it at a workspace, it
opens a rich TUI, and you drive a coding session through your favorite model
provider while Inductor owns the workspace tools (read, edit, grep, bash, …),
permission gating, diffs, and session history.

Under the hood Inductor pairs a Rust harness (execution, tools, persistence,
git worktrees) with an [OpenTUI](https://github.com/sst/opentui) frontend, and
talks to providers like **Claude** (via the Claude Agent SDK) and **Codex**.

---

## Requirements

Before running Inductor, make sure you have:

- **Rust** (stable toolchain, edition 2024) — <https://rustup.rs>
- **Bun** `>= 1.3` — <https://bun.sh> (only needed when building from source or developing the TUI)
- **Node.js + npm** — needed for the Claude provider's JS bridge
- A logged-in provider:
  - **Claude**: a working [Claude Code](https://docs.anthropic.com/en/docs/claude-code) login
    (run `claude` in your terminal once and sign in). Inductor reuses that
    user-level credential.
  - **Codex**: a Codex/OpenAI login (`~/.codex/auth.json`, or set `CODEX_HOME`).

You can confirm Inductor sees a credential with:

```sh
inductor auth detect
```

---

## Install

### Install a release bundle

Release bundles ship the CLI binary as `inductor` plus the self-contained
`inductor-open-tui` frontend.

Unpack a release archive, keep the binaries in the same directory, and add
that directory to your `PATH`. The CLI is available as `inductor`. `inductor open-tui`
will automatically launch the packaged frontend without requiring Bun.

Current release binaries support **Apple Silicon macOS only**.

Or install the latest GitHub Release directly:

```sh
curl -fsSL https://raw.githubusercontent.com/prayanshchhablani/inductor/main/scripts/install.sh | sh
```

### Build from source

Clone the repo and install dependencies:

```sh
git clone <repo-url> inductor
cd inductor

# JS/TUI dependencies (also wires up the Claude bridge via postinstall)
bun install

# Build the Rust binary and self-contained OpenTUI frontend
cargo build --release
INDUCTOR_TUI_OUTFILE=target/release/inductor-open-tui bun run build:tui
```

The compiled binaries land at `target/release/inductor` and
`target/release/inductor-open-tui`. Keep them side by side if you want `inductor open-tui`
to run without Bun. For a packaged archive, run:

```sh
bun run bundle:release
```

If you only want the Rust CLI on your `PATH`, you can still install it with:

```sh
cargo install --path crates/agent
```

---

## Usage

Start an interactive session in the current directory:

```sh
inductor open-tui
```

Common options:

```sh
# Pick a provider (defaults to claude)
inductor open-tui --provider codex

# Choose a workspace folder
inductor open-tui --workspace ./my-project

# Use a specific model
inductor open-tui --provider claude --model claude-sonnet-4-5

# Restrict file tools and bash to the workspace instead of yolo mode
inductor open-tui --workspace-only
```

### Approval modes

By default Inductor runs in **yolo mode** — it never pauses to ask before
running commands, edits, reads, or writes. To require approval before mutating
actions, pass an approval policy:

```sh
inductor open-tui --approval on_request
```

| Value        | Behavior                                            |
| ------------ | --------------------------------------------------- |
| `never`      | Default. Auto-run every tool (yolo).                |
| `on_request` | Ask before running mutating tools (edit/write/bash).|

Add `--workspace-only` to confine file tools and bash to the chosen workspace.

---

## How it works

- **Rust harness** (`crates/`) runs the turn loop, executes tools, enforces
  permissions, computes diffs, and persists sessions to a local SQLite DB
  (default: `<workspace>/.inductor/state.db`).
- **OpenTUI frontend** (`packages/tui`) renders the chat, tool activity, diffs,
  and terminal panes. It's launched automatically by `inductor open-tui`.
- **Providers** plug into a common interface:
  - `provider-claude` bridges to the Claude Agent SDK
    (`crates/provider-claude/js/claude_agent_sdk_bridge.mjs`).
  - `provider-codex` integrates Codex.

Inductor owns the workspace tools (read/edit/grep/bash/etc.) so the model's
actions are gated and rendered consistently, regardless of provider.

---

## Troubleshooting

- **`401 Invalid authentication credentials` (Claude):** make sure you've signed
  in with Claude Code (`claude` in the terminal). Inductor loads the *user*
  setting source for credentials.
- **`inductor auth detect` shows `status: none`:** no provider login was found.
  Sign in to Claude Code or Codex, then retry.
- **`OpenTUI dependencies are missing`:** if you're running from a source checkout, run `bun install` from the repo root.
- **`OpenTUI frontend is unavailable`:** either run `INDUCTOR_TUI_OUTFILE=target/release/inductor-open-tui bun run build:tui` from the repo root or use a release archive that includes `inductor-open-tui` next to `inductor`.

---

## Development

```sh
# One-time local hook setup
bash scripts/setup-git-hooks.sh

# Fast pre-commit gate
bash scripts/checks/pre-commit.sh

# Full pre-push / CI-equivalent gate
bash scripts/checks/pre-push.sh

# Build a distributable release bundle for the current platform
bun run bundle:release

# Run the TUI frontend directly during development
bun run tui

# Type-check and test the TUI
bun run typecheck
bun run test:tui

# Rust tests
cargo test
```

GitHub Actions lives under `.github/workflows/`:

- `ci.yml` runs Rust formatting, strict clippy, Rust tests, TUI type-checks/tests, and startup smoke checks on Ubuntu and macOS.
- `coverage.yml` records Rust coverage with a 66% line-coverage threshold and TUI coverage with a 70% line-coverage threshold.
- `pr-section-tests.yml` uses path filtering to run only the affected PR checks for tools, providers, and the OpenTUI frontend.
- `release.yml` builds tar.gz bundles containing `inductor` and the self-contained `inductor-open-tui`, uploads them as workflow artifacts, and publishes them to GitHub Releases on `v*` tags.

To enforce these before merge, configure branch protection in GitHub so the CI, coverage, and relevant section test checks are required status checks.
