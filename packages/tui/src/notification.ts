import type { TranscriptItem } from "./state"

const PREVIEW_MAX_CHARACTERS = 120

const NOTIFICATION_SCRIPT = [
  "on run argv",
  "display notification (item 2 of argv) with title (item 1 of argv)",
  "end run",
].join("\n")

type NotificationProcess = { unref(): unknown }
type NotificationSpawn = (
  command: string[],
  options: { stdout: "ignore"; stderr: "ignore" },
) => NotificationProcess

export type NotificationOptions = {
  platform?: string
  spawn?: NotificationSpawn
}

/**
 * Turn arbitrary assistant markdown into the compact plain-text preview used
 * by macOS notification banners. macOS performs the final visual two-line
 * clamp; this cap prevents a full response from being handed to Notification
 * Center and guarantees an explicit trailing ellipsis for longer output.
 */
export function notificationPreview(output: string, maxCharacters = PREVIEW_MAX_CHARACTERS) {
  const lines = output
    .split(/\r?\n/)
    .map((line) => line.replace(/\s+/g, " ").trim())
    .filter(Boolean)
  const compact = lines.slice(0, 2).join(" ")
  if (!compact || maxCharacters <= 0) return ""

  const characters = Array.from(compact)
  const truncated = lines.length > 2 || characters.length > maxCharacters
  if (!truncated) return compact
  if (maxCharacters <= 3) return ".".repeat(maxCharacters)

  return `${characters.slice(0, maxCharacters - 3).join("").trimEnd()}...`
}

/** Pick the user-visible output for a finished run, including useful errors. */
export function completionNotificationOutput(
  transcript: readonly TranscriptItem[],
  status: string,
  exitCode: number | null,
) {
  const failed = exitCode !== 0 || /^(?:error|failed|exited\b)/i.test(status)
  const preferredKinds = failed ? ["error", "assistant"] : ["assistant", "error"]

  for (const kind of preferredKinds) {
    for (let index = transcript.length - 1; index >= 0; index -= 1) {
      const item = transcript[index]
      if (item.kind === kind && "text" in item && item.text.trim()) return item.text
    }
  }

  return failed ? "Agent run failed." : "Agent run completed."
}

/** Send a best-effort native notification without delaying or failing the run. */
export function notifyAgentRunCompleted(
  sessionName: string,
  output: string,
  options: NotificationOptions = {},
) {
  if ((options.platform ?? process.platform) !== "darwin") return false

  const title = sessionName.trim() || "Inductor session"
  const body = notificationPreview(output) || "Agent run completed."
  const spawn: NotificationSpawn = options.spawn ?? ((command, spawnOptions) => Bun.spawn(command, spawnOptions))

  try {
    const child = spawn(
      ["osascript", "-e", NOTIFICATION_SCRIPT, "--", title, body],
      { stdout: "ignore", stderr: "ignore" },
    )
    child.unref()
    return true
  } catch {
    return false
  }
}
