import { describe, expect, test } from "bun:test"
import type { SessionEvent, StoredSessionDetail } from "../src/backend"
import { createInitialState } from "../src/state"
import {
  RECENT_SESSION_EVENT_LIMIT,
  appendSessionHistoryEvent,
  createSessionHistoryWindow,
  mergeSessionHistoryPage,
  releaseSessionHistory,
} from "../src/session_history"

describe("disk-backed session history", () => {
  test("prepends one page and rebuilds tool calls split across the cursor", () => {
    const detail = storedSessionDetail([{
      type: "tool_call_result",
      tool_call_id: "call-1",
      output: "older result",
      exit_code: 0,
    }], 10, 101)
    const history = createSessionHistoryWindow(detail)
    expect(history).toBeDefined()

    const expanded = mergeSessionHistoryPage(history!, {
      events: [{
        type: "tool_call_start",
        tool_call_id: "call-1",
        name: "bash",
        input_json: { command: "git status" },
      }],
      event_start_ordinal: 9,
      event_end_ordinal: 9,
      has_older: true,
    })

    expect(expanded.temporaryEvents).toHaveLength(1)
    expect(expanded.beforeOrdinal).toBe(9)
    expect(expanded.temporaryTranscript).toContainEqual(expect.objectContaining({
      kind: "tool",
      name: "bash",
      status: "done",
      output: "older result",
    }))
    expect(expanded.temporaryTranscript?.[0]).toMatchObject({
      kind: "status",
      text: expect.stringContaining("99 older events"),
    })
  })

  test("releases temporary pages and resets the cursor at the recent window", () => {
    const history = createSessionHistoryWindow(storedSessionDetail([{ type: "text_delta", text: "recent" }], 10, 100))!
    const expanded = mergeSessionHistoryPage(history, {
      events: [{ type: "text_delta", text: "older" }],
      event_start_ordinal: 9,
      event_end_ordinal: 9,
      has_older: false,
    })

    const released = releaseSessionHistory(expanded)

    expect(released.temporaryEvents).toEqual([])
    expect(released.temporaryTranscript).toBeUndefined()
    expect(released.beforeOrdinal).toBe(10)
    expect(released.hasOlder).toBe(true)
  })

  test("keeps the live recent window bounded while advancing its disk cursor", () => {
    let history = createSessionHistoryWindow(storedSessionDetail([{ type: "status", status: "idle" }], 50, 1_000))!
    for (let index = 0; index < RECENT_SESSION_EVENT_LIMIT + 25; index += 1) {
      history = appendSessionHistoryEvent(
        history,
        { type: "text_delta", text: String(index) },
        createInitialState(),
      )
    }

    expect(history.recentEvents).toHaveLength(RECENT_SESSION_EVENT_LIMIT)
    expect(history.recentStartOrdinal).toBe(76)
    expect(history.beforeOrdinal).toBe(76)
    expect(history.totalEventCount).toBe(2_025)
  })

  test("keeps live events adjoining a visible page, then caps them on release", () => {
    const events = Array.from({ length: RECENT_SESSION_EVENT_LIMIT }, (_, index) => ({
      type: "text_delta",
      text: String(index),
    }))
    let history = createSessionHistoryWindow(storedSessionDetail(events, 10, 2_000))!
    history = { ...history, loading: true }
    history = appendSessionHistoryEvent(history, { type: "text_delta", text: "live" }, createInitialState())

    expect(history.recentEvents).toHaveLength(RECENT_SESSION_EVENT_LIMIT + 1)
    expect(history.recentStartOrdinal).toBe(10)

    history = releaseSessionHistory(history)
    expect(history.recentEvents).toHaveLength(RECENT_SESSION_EVENT_LIMIT)
    expect(history.recentStartOrdinal).toBe(11)
    expect(history.beforeOrdinal).toBe(11)
  })
})

function storedSessionDetail(events: SessionEvent[], eventStartOrdinal: number, eventCount: number): StoredSessionDetail {
  return {
    session: {
      id: "s-history",
      provider_id: "codex",
      model: "gpt-5.6-sol",
      status: "completed",
      display_name: "History test",
      created_at: "2026-07-12T00:00:00Z",
      updated_at: "2026-07-12T00:00:00Z",
    },
    messages: [],
    events,
    event_count: eventCount,
    event_start_ordinal: eventStartOrdinal,
    event_end_ordinal: eventStartOrdinal + events.length - 1,
    events_truncated: true,
  }
}
