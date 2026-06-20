---
name: inductor
description: Understand and operate Inductor — how its workspaces, git worktrees, branches, tools, approval modes, providers, and review flow work. Use whenever you are running as an agent inside Inductor, or when helping someone set up, explain, or troubleshoot an Inductor session. Most importantly, use this to know that you should do all of your work inside the session's worktree, never the original checkout.
license: Proprietary
compatibility: Inductor is a terminal-native AI coding agent (Rust harness + OpenTUI frontend) that runs Claude, Codex, and Copilot agents locally in isolated git worktree workspaces.
---

# Inductor

Use this skill when you are operating as an agent inside Inductor, or when you
are helping a user understand, configure, or troubleshoot an Inductor session.

Inductor is a terminal-native AI coding agent. It opens a rich TUI, owns the
workspace tools (read, edit, grep, bash, …), gates permissions, computes diffs,
and persists session history, while talking to a model **provider**. The
providers are **Claude** (via the Claude Agent SDK), **Codex**, and **Copilot**.
Inductor owns the tools so your actions are gated and rendered consistently no
matter which provider is driving.

Only describe behavior that actually exists in Inductor. Do not invent settings,
providers, scripts, ports, cloud workspaces, or controls that the app does not
have. When unsure, say so.

## The single most important rule: work inside the worktree

By default Inductor runs each session inside its **own git worktree** — a
separate working directory on its own branch, created from the repository's
current commit. The original checkout (the "root" repo the user opened) is a
**different directory** on disk. Multiple agents can run in parallel, each in its
own worktree, without ever touching each other's files or the user's uncommitted
work in the root checkout.

Therefore:

-   **Do all of your work in the session's workspace (the worktree).** This is
    the directory reported as `Working directory` / `Workspace root` in your
    environment block, and it is where your tools are anchored.
-   **Never reach back into the original checkout to make edits.** Editing the
    root repository defeats isolation, can clobber the user's uncommitted
    changes, and produces changes that do not show up on your branch or in the
    diff viewer.
-   Use **relative paths**, or paths under the workspace root. Your file tools
    (`read_file`, `write_file`, `edit_file`, `multi_edit`, `glob`, `grep`,
    `apply_patch_*`) and `bash` are all anchored to the workspace root, and the
    process working directory is set to the worktree, so plain relative work
    stays inside it automatically.
-   If you genuinely need a file from the original checkout (e.g. a gitignored
    local file that was not carried into the worktree), **read** it by its
    absolute path, then write the result **into the worktree**. Do not edit the
    original.

If you find yourself about to edit a path that is clearly the source checkout
rather than the current workspace, stop and reconsider — that is almost always a
mistake.

## Core model

-   Inductor runs locally in your terminal. A **session** is one conversation
    with one provider/model, driven by Inductor's turn loop.
-   Inductor has two development modes:
    -   **worktree** mode (the default for new TUI sessions): the agent runs
        inside an isolated git worktree on its own branch, so parallel sessions
        never touch each other's files.
    -   **in-place** mode: the agent edits the given workspace directory
        directly. Use this only when isolation is not wanted.
-   New worktrees are created from the source repo's **current branch (HEAD)**.
    Creation is allowed even when the source checkout is dirty: the new worktree
    checks out HEAD and never touches the source checkout, so the user's
    uncommitted changes stay put.
-   Worktrees are laid out on disk as
    `~/inductor/workspaces/<repo>/<branch>` — the path mirrors the repository and
    the branch the session is about.
-   A new session's branch starts as a placeholder (slug `session`). After your
    first prompt is recorded, Inductor silently asks the provider for a short
    name and renames the **session, branch, and worktree directory** in place to
    something descriptive (e.g. `fix-login`). A numeric suffix is appended only
    on collision (`fix-login-2`). When this rename happens mid-run, Inductor
    moves the worktree directory and re-points your tools and working directory
    at the new path, so keep using relative paths and they will follow.
-   Session chat history is persisted in SQLite. The per-session database is kept
    **outside** the worktree directory, so archiving a worktree (which deletes
    its working directory) preserves the conversation.
-   A separate app-level database registers every managed worktree so the TUI
    dashboard can list, reopen, and archive them, and report drift.

## Tools

Inductor owns these tools and runs them itself (not the provider):

-   `read_file`, `list_dir` — read files and directories.
-   `write_file` — create or overwrite a whole file (parent dirs created).
-   `edit_file`, `multi_edit` — exact-substring replacements in an existing file.
    Prefer these over `write_file` when changing existing files. They reject
    binary files, stale `expected_hash` values, and non-unique matches.
-   `apply_patch_freeform`, `apply_patch_structured` — apply unified or
    structured patches (edit / multi-edit / rename).
-   `glob`, `grep` — find files by pattern, search text.
-   `web_fetch` — fetch a webpage's text.
-   `todo_write` — maintain a task list for multi-step work.
-   `bash`, `bash_wait`, `bash_kill` — run shell commands. Long-running commands
    return a checkpoint with a `command_id`; use `bash_wait` to keep waiting or
    `bash_kill` to stop them.

All file tools and `bash` are anchored to the workspace root (the worktree). Use
them with relative paths and your work lands inside the session's branch.

## Approval modes

Inductor gates mutating tool calls according to an approval policy:

-   `never` — the default ("yolo"): never pause; auto-run every tool.
-   `on_request` — ask before running mutating tools (edit/write/bash).
-   `mutating` — ask before any state-changing tool.
-   `on_failure` — ask after a failure.
-   `always` — ask before every tool.

Independently, execution can be **unrestricted** (default) or **workspace-only**:

-   **Unrestricted / yolo** (default): file tools may resolve absolute and `..`
    paths outside the workspace, and `bash` runs without the OS sandbox. Even so,
    the *preferred* and default home is the worktree — staying inside it is the
    right behavior; reaching outside should be deliberate and rare.
-   **workspace-only** (`--workspace-only`): file tools and `bash` are confined
    to the workspace. On macOS this is enforced with a `sandbox-exec` (Seatbelt)
    profile; on other platforms the confinement is best-effort.

When helping a user, explain that workspace-only does not change *where you
should* work (always the worktree) — it changes whether reaching outside is even
*possible*.

## Providers, models, and reasoning

-   Providers: **Claude** (Claude Agent SDK), **Codex**, **Copilot**. Inductor
    reuses the provider's existing local login (e.g. a Claude Code login, Codex's
    `~/.codex/auth.json`). `inductor auth detect` reports what credentials
    Inductor can see.
-   The model is provider-specific and selectable per session.
-   Reasoning effort can be tuned per session: `none`, `minimal`, `low`,
    `medium`, `high`, `xhigh`, `max`. Higher effort spends more reasoning budget.
-   Inductor manages context length itself: it counts tokens and compacts older
    transcript when soft/hard limits are exceeded, and offloads very large tool
    outputs to a blob store. You do not need to manage this manually.

## Parallel agents and worktree lifecycle

-   The TUI can run **multiple concurrent agents**, each in its own worktree and
    branch, working the same repo in parallel. This is the whole reason worktree
    mode is the default — isolation makes parallelism safe.
-   **Drift**: Inductor can report how far the target branch has advanced since a
    worktree was created, so a user knows when a branch is behind.
-   **Archive**: archiving a worktree removes its working directory but keeps the
    registry record and the session's chats. Reopening an archived session is
    read-only because the working directory is gone.

## Review and merge

-   Inductor renders a **diff viewer** of your changes and supports per-hunk
    review (accept/reject). Because your work is on its own branch in its own
    worktree, the diff is exactly what you changed — another reason to never edit
    the original checkout.
-   When you finish, summarize what changed and how you verified it, and prefer a
    validation step (build/test) run from inside the workspace that exercises the
    riskiest part of the change.

## Operating guidance for agents

-   Inspect relevant files before changing code; do not guess structure.
-   Prefer the smallest correct change that follows existing local patterns.
-   Never revert or overwrite unrelated user changes — and remember the user's
    uncommitted changes live in the *original* checkout, which your worktree does
    not include, so do not assume they are present and do not go editing them.
-   Keep all edits inside the workspace/worktree. Use relative paths.
-   Run focused verification from inside the workspace when practical.
-   State the outcome and any verification performed, concisely.

## Troubleshooting

-   **Edits "disappeared" or did not show in the diff:** confirm you edited the
    workspace (worktree), not the original checkout. The diff viewer only shows
    the worktree branch.
-   **`ENOENT` / file-not-found right after a session starts:** the placeholder
    worktree may have just been renamed and moved. Re-resolve paths relative to
    the (new) workspace root rather than caching an absolute path from earlier.
-   **A gitignored local file (e.g. `.env`) is missing in the worktree:** a fresh
    worktree checks out tracked files at HEAD; untracked/gitignored files from
    the original checkout are not copied. Recreate or copy what you need into the
    worktree rather than editing the original.
-   **`inductor auth detect` shows no credential:** the provider is not logged in
    locally. Sign in to Claude Code / Codex / Copilot and retry.
-   **Workspace-only command failed but works manually:** in workspace-only mode
    the command is sandboxed to the workspace; a path or network access it needs
    is being denied. Either keep it inside the workspace or run unrestricted.
-   **Archived session is read-only:** its worktree directory was removed on
    archive; the chats are kept but there is no working tree to edit.
