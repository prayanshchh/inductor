import { describe, expect, test } from "bun:test"
import { insertTextAtCursor, recordPromptHistory, shouldNavigateHistory, stepPromptHistory, type PromptHistoryState } from "../src/prompt_input"

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
})