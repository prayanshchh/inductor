import { describe, expect, test } from "bun:test"
import { applyPermissionDecision, applySessionEvent, addUserMessage, createInitialState, loadStoredSession } from "../src/state"

describe("transcript reducer", () => {
  test("streams assistant text into a single row", () => {
    let state = createInitialState()
    state = applySessionEvent(state, { type: "text_delta", text: "hello" })
    state = applySessionEvent(state, { type: "text_delta", text: " world" })

    expect(state.transcript).toEqual([{ id: expect.any(String), kind: "assistant", text: "hello world" }])
  })

  test("keeps permission requests inline in state", () => {
    let state = addUserMessage(createInitialState(), "edit the file")
    state = applySessionEvent(state, {
      type: "permission_request",
      request_id: "req-1",
      tool_name: "write_file",
      reason: "mutating tool",
      input_json: { path: "src/main.rs" },
    })

    expect(state.pendingPermission?.requestId).toBe("req-1")
    expect(state.pendingPermission?.input).toContain("src/main.rs")

    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-approve",
      name: "write_file",
      input_json: { path: "src/main.rs", content: "fn main() {}\n" },
    })
    state = applyPermissionDecision(state, "allow")
    expect(state.pendingPermission).toBeUndefined()
    expect(state.transcript.at(-1)).toMatchObject({ kind: "tool", approval: "allow" })
    expect(state.transcript.some((item) => item.kind === "status" && item.text === "Allowed once")).toBe(false)
  })

  test("derives permission request diffs from write file content", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "permission_request",
      request_id: "req-1",
      tool_name: "write_file",
      reason: "mutating tool",
      input_json: { path: "README.md", content: "# Inductor\n\nOverview\n" },
    })

    expect(state.pendingPermission?.filepath).toBe("README.md")
    expect(state.pendingPermission?.diff).toContain("+++ b/README.md")
    expect(state.pendingPermission?.diff).toContain("+# Inductor")
  })

  test("applies permission coloring to the later tool row when approval resolves first", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "permission_request",
      request_id: "req-1",
      tool_name: "write_file",
      reason: "mutating tool",
      input_json: { path: "src/main.rs", content: "fn main() {}\n" },
    })
    state = applyPermissionDecision(state, "allow")
    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-approve",
      name: "write_file",
      input_json: { path: "src/main.rs", content: "fn main() {}\n" },
    })

    expect(state.transcript.at(-1)).toMatchObject({ kind: "tool", approval: "allow" })
    expect(state.permissionApprovals).toEqual([])
  })

  test("updates tool rows by call id", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-1",
      name: "grep",
      input_json: { pattern: "TODO" },
    })
    state = applySessionEvent(state, {
      type: "tool_call_result",
      tool_call_id: "call-1",
      output: "src/main.rs:1:TODO",
      exit_code: 0,
    })

    expect(state.transcript[0]).toMatchObject({
      kind: "tool",
      status: "done",
      output: "src/main.rs:1:TODO",
    })
  })

  test("does not mark read-only tool paths as modified files", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-read",
      name: "read_file",
      input_json: { path: "CONTEXT.md" },
    })

    expect(state.modifiedFiles).toEqual([])
  })

  test("tracks only mutating tool paths as modified files", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-write",
      name: "write_file",
      input_json: { path: "src/main.rs", content: "fn main() {}\n" },
    })

    expect(state.modifiedFiles).toEqual([
      expect.objectContaining({ file: "src/main.rs", additions: 1, deletions: 0 }),
    ])
  })

  test("carries patch event diffs with modified files", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-edit",
      name: "edit_file",
      input_json: { path: "src/main.rs", old: "old", new: "new" },
    })
    state = applySessionEvent(state, {
      type: "patch",
      files: [{ path: "src/main.rs", additions: 1, deletions: 1, diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n" }],
    })

    expect(state.modifiedFiles[0]).toMatchObject({
      file: "src/main.rs",
      additions: 1,
      deletions: 1,
      diff: expect.stringContaining("+new"),
    })
    expect(state.transcript.at(-1)).toMatchObject({
      kind: "tool",
      diff: expect.stringContaining("+new"),
    })
  })

  test("keeps diagnostics metadata out of the transcript", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "diagnostics",
      files: [{ path: "src/main.rs", exists: true, lines: 7, bytes: 120 }],
    })

    expect(state.transcript).toEqual([])
  })

  test("renders requested tool events as tool rows", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "tool_call_requested",
      tool_call_id: "call-read",
      name: "read_file",
      input_json: { path: "CONTEXT.md" },
    })

    expect(state.transcript).toHaveLength(1)
    expect(state.transcript[0]).toMatchObject({
      kind: "tool",
      toolCallId: "call-read",
      name: "read_file",
      input: '{\n  "path": "CONTEXT.md"\n}',
      status: "running",
    })
  })

  test("does not duplicate requested tool when the start event arrives", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "tool_call_requested",
      name: "read_file",
      input_json: { path: "CONTEXT.md" },
    })
    state = applySessionEvent(state, {
      type: "tool_call_start",
      tool_call_id: "call-read",
      name: "read_file",
      input_json: { path: "CONTEXT.md" },
    })

    expect(state.transcript).toHaveLength(1)
    expect(state.transcript[0]).toMatchObject({ kind: "tool", toolCallId: "call-read" })
  })

  test("parses terminal tool request text as a tool row", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "terminal_output",
      chunk: 'tool call requested: {"input":{"path":"CONTEXT.md"},"name":"read_file"}\n',
    })

    expect(state.transcript).toHaveLength(1)
    expect(state.transcript[0]).toMatchObject({
      kind: "tool",
      name: "read_file",
      input: '{\n  "path": "CONTEXT.md"\n}',
    })
  })

  test("loads stored sessions without generic status tool errors", () => {
    const state = loadStoredSession({
      session: {
        id: "s1",
        provider_id: "claude",
        model: "sonnet",
        status: "idle",
        display_name: null,
        created_at: "2026-06-13T00:00:00Z",
        updated_at: "2026-06-13T00:00:00Z",
      },
      messages: [
        { role: "user", content: "read context", ordinal: 0 },
        { role: "assistant", content: 'tool call requested: {"input":{"path":"CONTEXT.md"},"name":"read_file"}', ordinal: 1 },
        { role: "tool", content: "Tool: read_file error: tool paths must be workspace-relative", ordinal: 2 },
        { role: "assistant", content: "Done.", ordinal: 3 },
      ],
    })

    expect(state.transcript.map((item) => item.kind)).toEqual(["user", "tool", "assistant"])
    expect(state.transcript[1]).toMatchObject({ kind: "tool", name: "read_file" })
  })

  test("loads stored sessions from event order instead of clubbing assistant text", () => {
    const state = loadStoredSession({
      session: {
        id: "s1",
        provider_id: "claude",
        model: "sonnet",
        status: "completed",
        display_name: null,
        created_at: "2026-06-13T00:00:00Z",
        updated_at: "2026-06-13T00:00:00Z",
      },
      messages: [
        { role: "user", content: "do work", ordinal: 0 },
        { role: "assistant", content: "initial thoughts\nmiddle thoughts\nfinal thoughts", ordinal: 1 },
      ],
      events: [
        { type: "text_delta", text: "initial thoughts" },
        { type: "tool_call_start", tool_call_id: "call-1", name: "read_file", input_json: { path: "a.rs" } },
        { type: "tool_call_result", tool_call_id: "call-1", output: "a", exit_code: 0 },
        { type: "text_delta", text: "middle thoughts" },
        { type: "tool_call_start", tool_call_id: "call-2", name: "bash", input_json: { command: "cargo test" } },
        { type: "tool_call_result", tool_call_id: "call-2", output: "ok", exit_code: 0 },
        { type: "text_delta", text: "final thoughts" },
        { type: "result", stop_reason: "end_turn" },
      ],
    })

    expect(state.transcript.map((item) => item.kind)).toEqual([
      "user",
      "assistant",
      "tool",
      "assistant",
      "tool",
      "assistant",
    ])
    expect(state.transcript[0]).toMatchObject({ kind: "user", text: "do work" })
    expect(state.transcript[1]).toMatchObject({ kind: "assistant", text: "initial thoughts" })
    expect(state.transcript[3]).toMatchObject({ kind: "assistant", text: "middle thoughts" })
    expect(state.transcript[5]).toMatchObject({ kind: "assistant", text: "final thoughts" })
  })

  test("replays repeated user prompts from durable user message events", () => {
    const state = loadStoredSession({
      session: {
        id: "s1",
        provider_id: "codex",
        model: "gpt-5.5",
        status: "streaming",
        display_name: null,
        created_at: "2026-06-13T00:00:00Z",
        updated_at: "2026-06-13T00:00:00Z",
      },
      messages: [
        { role: "user", content: "hi", ordinal: 0 },
        { role: "assistant", content: "Hi! What would you like me to work on?", ordinal: 1 },
        { role: "user", content: "do all the tools calls availble to u just to check their reliability", ordinal: 2 },
      ],
      events: [
        { type: "user_message", text: "hi" },
        { type: "text_delta", text: "Hi! What would you like me to work on?" },
        { type: "tool_call_requested", tool_call_id: "call-1", name: "list_dir", input_json: {} },
        { type: "tool_call_error", tool_call_id: "call-1", message: "list_dir failed" },
        { type: "user_message", text: "do all the tools calls availble to u just to check their reliability" },
        { type: "tool_call_requested", tool_call_id: "call-2", name: "list_dir", input_json: {} },
      ],
    })

    expect(state.transcript.map((item) => item.kind)).toEqual([
      "user",
      "assistant",
      "tool",
      "user",
      "tool",
    ])
    expect(state.transcript[3]).toMatchObject({
      kind: "user",
      text: "do all the tools calls availble to u just to check their reliability",
    })
  })

  test("keeps old first prompt when later turns have user message events", () => {
    const state = loadStoredSession({
      session: {
        id: "s1",
        provider_id: "codex",
        model: "gpt-5.5",
        status: "streaming",
        display_name: null,
        created_at: "2026-06-13T00:00:00Z",
        updated_at: "2026-06-13T00:00:00Z",
      },
      messages: [
        { role: "user", content: "hi", ordinal: 0 },
        { role: "assistant", content: "Hello.", ordinal: 1 },
        { role: "user", content: "do all the tools calls availble to u just to check their reliability", ordinal: 2 },
      ],
      events: [
        { type: "text_delta", text: "Hello." },
        { type: "result", stop_reason: "end_turn" },
        { type: "user_message", text: "do all the tools calls availble to u just to check their reliability" },
        { type: "tool_call_requested", tool_call_id: "call-2", name: "list_dir", input_json: {} },
      ],
    })

    expect(state.transcript.map((item) => item.kind)).toEqual(["user", "assistant", "user", "tool"])
    expect(state.transcript[0]).toMatchObject({ kind: "user", text: "hi" })
    expect(state.transcript[2]).toMatchObject({
      kind: "user",
      text: "do all the tools calls availble to u just to check their reliability",
    })
  })

  test("shows stopped agent for interrupted results", () => {
    let state = addUserMessage(createInitialState(), "do a long task")
    state = applySessionEvent(state, { type: "result", stop_reason: "interrupted" })

    expect(state.running).toBe(false)
    expect(state.status).toBe("stopped")
    expect(state.transcript.at(-1)).toMatchObject({ kind: "error", text: "stopped agent" })

    state = applySessionEvent(state, { type: "result", stop_reason: "interrupted" })
    expect(state.transcript.filter((item) => item.kind === "error" && item.text === "stopped agent")).toHaveLength(1)
  })
})
