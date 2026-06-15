# Inductor

A Rust workspace with a TypeScript TUI for the Inductor coding agent — a session-based AI coding assistant with worktree isolation, multi-provider support, and sandboxed tool execution.

## Repository Structure

```
krakow/
├── crates/       # Rust backend crates
└── packages/     # TypeScript packages (TUI)
```

## Workspace Crates

### Backend (Rust)

| Crate | Description |
|---|---|
| `agent` | Core agent harness and session orchestration |
| `harness-core` | Shared harness types and interfaces |
| `harness-runtime` | Runtime execution environment |
| `provider-core` | Provider abstraction and shared types |
| `provider-claude` | Claude (Anthropic) provider integration |
| `provider-codex` | Codex provider integration |
| `tools` | Agent tool implementations |
| `context` | Context management and summarization |
| `persistence` | Session and state persistence |
| `git` | Git operations and worktree management |
| `diff` | Diff rendering and patch utilities |
| `sandbox` | Sandboxed execution environment |
| `terminal` | Terminal/TUI interface |
| `auth` | Authentication utilities |
| `session-naming` | Session name generation |

### Frontend (TypeScript)

| Package | Description |
|---|---|
| `@inductor/tui` | Terminal UI built with Solid.js and OpenTUI |

## Getting Started

```bash
# Build all Rust crates
cargo build

# Run Rust tests
cargo test

# Run the TUI (development)
cd packages/tui && bun run dev

# Type-check the TUI
cd packages/tui && bun run typecheck
```

## Architecture

Sessions run in isolated git worktrees. The agent state database lives outside the worktree so that session history is preserved across merges and archive operations. Providers (Claude, Codex) are swappable at the harness level.
