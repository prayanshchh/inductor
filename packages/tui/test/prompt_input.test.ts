import { describe, expect, test } from "bun:test"
import { insertTextAtCursor, parsePromptHistory, PROMPT_HISTORY_LIMIT, recordPromptHistory, serializePromptHistory, shouldCompactPastedText, shouldNavigateHistory, stepPromptHistory, type PromptHistoryState } from "../src/prompt_input"

describe("prompt input helpers", () => {
  test("inserts ctrl-j newline at the cursor", () => {
    expect(insertTextAtCursor("hello world", "\n", 5)).toEqual({ value: "hello\n world", cursorOffset: 6 })
  })

  test("inserts ctrl-j newline into an empty prompt", () => {
    expect(insertTextAtCursor("", "\n", 0)).toEqual({ value: "\n", cursorOffset: 1 })
  })

  test("navigates history from the current draft and restores it", () => {
    let state: PromptHistoryState = { entries: [] as string[], draft: "" }
    state = { ...state, entries: recordPromptHistory(state.entries, "first") }
    state = { ...state, entries: recordPromptHistory(state.entries, "second") }

    let result = stepPromptHistory(state, "unfinished", -1)
    expect(result.value).toBe("second")

    result = stepPromptHistory(result.state, result.value, -1)
    expect(result.value).toBe("first")

    result = stepPromptHistory(result.state, result.value, 1)
    expect(result.value).toBe("second")

    result = stepPromptHistory(result.state, result.value, 1)
    expect(result.value).toBe("unfinished")
  })

  test("keeps arrow keys inside multiline prompts until the boundary", () => {
    const prompt = "top\nbottom"
    expect(shouldNavigateHistory(prompt, 5, -1)).toBe(false)
    expect(shouldNavigateHistory(prompt, 2, -1)).toBe(true)
    expect(shouldNavigateHistory(prompt, 2, 1)).toBe(false)
    expect(shouldNavigateHistory(prompt, prompt.length, 1)).toBe(true)
  })

  test("round-trips persisted history and survives a reload", () => {
    const entries = recordPromptHistory(recordPromptHistory([], "first"), "second")
    const restored = parsePromptHistory(serializePromptHistory(entries))
    expect(restored).toEqual(["first", "second"])

    let state: PromptHistoryState = { entries: restored, draft: "" }
    const result = stepPromptHistory(state, "unfinished", -1)
    expect(result.value).toBe("second")
  })

  test("ignores malformed or non-string history payloads", () => {
    expect(parsePromptHistory("not json")).toEqual([])
    expect(parsePromptHistory("{}")).toEqual([])
    expect(parsePromptHistory(JSON.stringify(["ok", 42, "", "  ", "fine"]))).toEqual(["ok", "fine"])
  })

  test("caps persisted history at the limit", () => {
    const entries = Array.from({ length: PROMPT_HISTORY_LIMIT + 50 }, (_, i) => `cmd ${i}`)
    const restored = parsePromptHistory(serializePromptHistory(entries))
    expect(restored.length).toBe(PROMPT_HISTORY_LIMIT)
    expect(restored.at(-1)).toBe(`cmd ${PROMPT_HISTORY_LIMIT + 49}`)
  })

  test("compacts only medium-long pasted text", () => {
    expect(shouldCompactPastedText("short paste")).toBe(false)
    expect(shouldCompactPastedText("x".repeat(501))).toBe(true)
    expect(shouldCompactPastedText(Array.from({ length: 9 }, (_, i) => `line ${i}`).join("\n"))).toBe(true)
  })
})
