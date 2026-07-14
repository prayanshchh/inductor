import { describe, expect, test } from "bun:test"
import {
  completionNotificationOutput,
  notificationPreview,
  notifyAgentRunCompleted,
} from "../src/notification"

describe("agent completion notifications", () => {
  test("collapses multiline output into a compact banner preview", () => {
    expect(notificationPreview("Implemented notifications.\n\nTests are passing.")).toBe(
      "Implemented notifications. Tests are passing.",
    )
  })

  test("shows only the first two non-empty output lines", () => {
    expect(notificationPreview("First line\nSecond line\nThird line")).toBe(
      "First line Second line...",
    )
  })

  test("truncates long Unicode-safe previews with three dots", () => {
    const preview = notificationPreview(`Done ${"🚀".repeat(100)}`, 20)

    expect(Array.from(preview)).toHaveLength(20)
    expect(preview).toEndWith("...")
    expect(preview).not.toContain("�")
  })

  test("uses the latest assistant output, or the latest error for a failed run", () => {
    const transcript = [
      { id: "user", kind: "user" as const, text: "build it" },
      { id: "assistant", kind: "assistant" as const, text: "Implemented the feature." },
      { id: "error", kind: "error" as const, text: "Provider connection failed." },
    ]

    expect(completionNotificationOutput(transcript, "end_turn", 0)).toBe("Implemented the feature.")
    expect(completionNotificationOutput(transcript, "error", 0)).toBe("Provider connection failed.")
  })

  test("passes the session name and preview as data arguments to osascript", () => {
    const calls: Array<{ command: string[]; options: unknown }> = []
    let unrefCalled = false

    const sent = notifyAgentRunCompleted('Session "quoted"', `First line\n${"x".repeat(200)}`, {
      platform: "darwin",
      spawn(command, options) {
        calls.push({ command, options })
        return { unref: () => { unrefCalled = true } }
      },
    })

    expect(sent).toBe(true)
    expect(calls).toHaveLength(1)
    expect(calls[0].command.slice(0, 4)).toEqual(["osascript", "-e", expect.any(String), "--"])
    expect(calls[0].command[4]).toBe('Session "quoted"')
    expect(calls[0].command[5]).toEndWith("...")
    expect(calls[0].command[2]).not.toContain('Session "quoted"')
    expect(unrefCalled).toBe(true)
  })

  test("does nothing outside macOS", () => {
    let spawned = false
    const sent = notifyAgentRunCompleted("Session", "Done", {
      platform: "linux",
      spawn() {
        spawned = true
        return { unref() {} }
      },
    })

    expect(sent).toBe(false)
    expect(spawned).toBe(false)
  })
})
