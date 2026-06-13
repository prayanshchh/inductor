export type HistoryDirection = -1 | 1

export type PromptHistoryState = {
  entries: string[]
  index?: number
  draft: string
}

export function insertTextAtCursor(value: string, insert: string, cursorOffset = value.length) {
  const offset = clampCursor(value, cursorOffset)
  const next = `${value.slice(0, offset)}${insert}${value.slice(offset)}`
  return { value: next, cursorOffset: offset + insert.length }
}

export function shouldNavigateHistory(value: string, cursorOffset: number, direction: HistoryDirection) {
  const offset = clampCursor(value, cursorOffset)
  if (direction < 0) return value.lastIndexOf("\n", Math.max(0, offset - 1)) === -1
  return value.indexOf("\n", offset) === -1
}

export const PROMPT_HISTORY_LIMIT = 500

export function recordPromptHistory(entries: string[], value: string) {
  const item = value.trim()
  if (!item) return entries
  if (entries.at(-1) === item) return entries
  return [...entries, item]
}

export function parsePromptHistory(raw: string): string[] {
  try {
    const data = JSON.parse(raw)
    if (!Array.isArray(data)) return []
    return data.filter((item): item is string => typeof item === "string" && item.trim().length > 0)
  } catch {
    return []
  }
}

export function serializePromptHistory(entries: string[]): string {
  return JSON.stringify(entries.slice(-PROMPT_HISTORY_LIMIT))
}

export function stepPromptHistory(state: PromptHistoryState, currentDraft: string, direction: HistoryDirection) {
  if (state.entries.length === 0) return { state, value: currentDraft, moved: false }

  const draft = state.index === undefined ? currentDraft : state.draft
  let index: number | undefined
  if (direction < 0) {
    index = state.index === undefined ? state.entries.length - 1 : Math.max(0, state.index - 1)
  } else {
    if (state.index === undefined) return { state, value: currentDraft, moved: false }
    index = state.index >= state.entries.length - 1 ? undefined : state.index + 1
  }

  const nextState = { entries: state.entries, index, draft }
  const value = index === undefined ? draft : state.entries[index]
  return { state: nextState, value, moved: value !== currentDraft || state.index !== index }
}

function clampCursor(value: string, cursorOffset: number) {
  if (!Number.isFinite(cursorOffset)) return value.length
  return Math.max(0, Math.min(value.length, cursorOffset))
}