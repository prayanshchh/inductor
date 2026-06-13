import { describe, expect, test } from "bun:test"
import { applyPermissionDecision, applySessionEvent, addUserMessage, createInitialState } from "../src/state"

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

  test("renders diagnostics metadata as a status row", () => {
    let state = createInitialState()
    state = applySessionEvent(state, {
      type: "diagnostics",
      files: [{ path: "src/main.rs", exists: true, lines: 7, bytes: 120 }],
    })

    expect(state.transcript.at(-1)).toMatchObject({
      kind: "status",
      text: "diagnostics: src/main.rs: 7 lines, 120 bytes",
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
