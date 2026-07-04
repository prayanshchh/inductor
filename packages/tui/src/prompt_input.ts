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

export type PromptPlaceholder = { label: string; replacement: string }

export function deletePromptPlaceholderAtCursor(value: string, cursorOffset: number, placeholders: readonly PromptPlaceholder[], direction: "backward" | "forward") {
  const offset = clampCursor(value, cursorOffset)
  const spans = placeholderSpans(value, placeholders)
  for (const span of spans) {
    if (direction === "backward") {
      const afterTokenSpace = offset === span.end + 1 && /\s/.test(value[span.end] ?? "")
      const deletingTokenEnd = offset === span.end - 1 && value[offset] === span.label.at(-1)
      if ((offset > span.start && offset <= span.end) || deletingTokenEnd || afterTokenSpace) {
        const end = afterTokenSpace ? span.end + 1 : span.end
        return { value: `${value.slice(0, span.start)}${value.slice(end)}`, cursorOffset: span.start, deleted: true }
      }
    } else if (offset >= span.start && offset < span.end) {
      return { value: `${value.slice(0, span.start)}${value.slice(span.end)}`, cursorOffset: span.start, deleted: true }
    }
  }
  return { value, cursorOffset: offset, deleted: false }
}

export function expandPromptPlaceholders(value: string, placeholders: readonly PromptPlaceholder[]) {
  if (placeholders.length === 0) return value
  let expanded = value
  for (const placeholder of placeholders) {
    expanded = expanded.split(placeholder.label).join(placeholder.replacement)
  }
  return expanded
}

export function shouldNavigateHistory(value: string, cursorOffset: number, direction: HistoryDirection) {
  const offset = clampCursor(value, cursorOffset)
  if (direction < 0) return value.lastIndexOf("\n", Math.max(0, offset - 1)) === -1
  return value.indexOf("\n", offset) === -1
}

export const PROMPT_HISTORY_LIMIT = 500
const COMPACT_PASTE_LENGTH = 500
const COMPACT_PASTE_LINES = 8

export function shouldCompactPastedText(text: string) {
  return text.length > COMPACT_PASTE_LENGTH || text.split("\n").length > COMPACT_PASTE_LINES
}

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

function placeholderSpans(value: string, placeholders: readonly PromptPlaceholder[]) {
  const spans: { start: number; end: number; label: string }[] = []
  for (const placeholder of placeholders) {
    if (!placeholder.label) continue
    let start = value.indexOf(placeholder.label)
    while (start >= 0) {
      spans.push({ start, end: start + placeholder.label.length, label: placeholder.label })
      start = value.indexOf(placeholder.label, start + placeholder.label.length)
    }
  }
  return spans.sort((a, b) => a.start - b.start)
}
