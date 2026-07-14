import type { SessionEvent, StoredSessionDetail, StoredSessionEventPage } from "./backend"
import { applySessionEvent, replayStoredSessionTranscript, type AppState, type TranscriptItem } from "./state"

export const RECENT_SESSION_EVENT_LIMIT = 1_000

/**
 * Durable history stays in SQLite. Only `recentEvents` is retained normally;
 * `temporaryEvents` and `temporaryTranscript` exist while the user is
 * deliberately browsing older pages.
 */
export type SessionHistoryWindow = {
  recentEvents: SessionEvent[]
  temporaryEvents: SessionEvent[]
  recentStartOrdinal: number
  beforeOrdinal: number
  hasOlder: boolean
  totalEventCount: number
  temporaryTranscript?: TranscriptItem[]
  loading: boolean
}

export function createSessionHistoryWindow(detail: StoredSessionDetail): SessionHistoryWindow | undefined {
  const recentEvents = detail.events ?? []
  const recentStartOrdinal = detail.event_start_ordinal
  if (!detail.events_truncated || !recentEvents.length || typeof recentStartOrdinal !== "number") return undefined
  return {
    recentEvents: [...recentEvents],
    temporaryEvents: [],
    recentStartOrdinal,
    beforeOrdinal: recentStartOrdinal,
    hasOlder: true,
    totalEventCount: Number(detail.event_count ?? recentEvents.length),
    loading: false,
  }
}

export function mergeSessionHistoryPage(
  history: SessionHistoryWindow,
  page: StoredSessionEventPage,
): SessionHistoryWindow {
  const temporaryEvents = [...page.events, ...history.temporaryEvents]
  const events = [...temporaryEvents, ...history.recentEvents]
  return {
    ...history,
    temporaryEvents,
    beforeOrdinal: typeof page.event_start_ordinal === "number"
      ? page.event_start_ordinal
      : history.beforeOrdinal,
    hasOlder: page.has_older,
    temporaryTranscript: replayStoredSessionTranscript(events, history.totalEventCount, page.has_older),
    loading: false,
  }
}

/** Release all temporary pages after returning to the live bottom. */
export function releaseSessionHistory(history: SessionHistoryWindow): SessionHistoryWindow {
  const recentEvents = [...history.recentEvents]
  const dropped = Math.max(0, recentEvents.length - RECENT_SESSION_EVENT_LIMIT)
  if (dropped > 0) recentEvents.splice(0, dropped)
  const recentStartOrdinal = history.recentStartOrdinal + dropped
  return {
    ...history,
    recentEvents,
    recentStartOrdinal,
    temporaryEvents: [],
    temporaryTranscript: undefined,
    beforeOrdinal: recentStartOrdinal,
    // A truncated recent window always has durable events before its cursor,
    // even if the user temporarily paged all the way to ordinal zero.
    hasOlder: true,
    loading: false,
  }
}

/** Keep the recent replay window bounded as new live events arrive. */
export function appendSessionHistoryEvent(
  history: SessionHistoryWindow,
  event: SessionEvent,
  baseState: AppState,
): SessionHistoryWindow {
  const recentEvents = [...history.recentEvents, event]
  // While a page is loading or visible, keep its adjoining recent events so
  // live provider output cannot create a cursor gap. Release compacts back to
  // the normal cap when the user returns to the bottom.
  const browsingHistory = history.loading || Boolean(history.temporaryTranscript)
  const dropped = browsingHistory ? 0 : Math.max(0, recentEvents.length - RECENT_SESSION_EVENT_LIMIT)
  if (dropped > 0) recentEvents.splice(0, dropped)
  const recentStartOrdinal = history.recentStartOrdinal + dropped
  const temporaryTranscript = history.temporaryTranscript
    ? applySessionEvent({ ...baseState, transcript: history.temporaryTranscript }, event).transcript
    : undefined
  return {
    ...history,
    recentEvents,
    recentStartOrdinal,
    beforeOrdinal: history.temporaryTranscript ? history.beforeOrdinal : recentStartOrdinal,
    totalEventCount: history.totalEventCount + 1,
    temporaryTranscript,
  }
}
