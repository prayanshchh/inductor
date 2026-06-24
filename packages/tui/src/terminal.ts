import type { BackendOptions } from "./backend"

export type TerminalSnapshot = {
  contents: string
  screen_rows?: string[]
  cursor_row: number
  cursor_col: number
  rows: number
  cols: number
  is_running: boolean
}

export type TerminalSize = { rows: number; cols: number }

export type TerminalSession = {
  /** Send raw bytes (e.g. a typed line ending in "\n") to the shell. */
  write(data: string): void
  /** Tell the PTY its new viewport size. */
  resize(size: TerminalSize): void
  /** Terminate the shell and the backend process. */
  kill(): void
}

export type TerminalCallbacks = {
  onSnapshot(snapshot: TerminalSnapshot): void
  onExit(code: number | null): void
}

const decoder = new TextDecoder()
const encoder = new TextEncoder()

/**
 * Spawn a persistent interactive shell in `cwd` via the backend's
 * `terminal serve` subcommand and stream its screen snapshots back.
 */
export function spawnTerminalSession(
  options: Pick<BackendOptions, "backendBin" | "repoRoot">,
  cwd: string,
  size: TerminalSize,
  callbacks: TerminalCallbacks,
): TerminalSession {
  const proc = Bun.spawn(
    [
      options.backendBin,
      "terminal",
      "serve",
      "--workspace",
      cwd,
      "--rows",
      String(Math.max(1, size.rows)),
      "--cols",
      String(Math.max(1, size.cols)),
    ],
    {
      cwd: options.repoRoot,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    },
  )

  void readSnapshots(proc.stdout, callbacks.onSnapshot)
  void proc.exited.then((code) => callbacks.onExit(code))

  function send(message: unknown) {
    try {
      proc.stdin.write(encoder.encode(`${JSON.stringify(message)}\n`))
      proc.stdin.flush()
    } catch {
      // process may have exited; ignore
    }
  }

  return {
    write(data) {
      send({ type: "input", data })
    },
    resize(next) {
      send({ type: "resize", rows: Math.max(1, next.rows), cols: Math.max(1, next.cols) })
    },
    kill() {
      try {
        proc.kill()
      } catch {
        // already gone
      }
    },
  }
}

async function readSnapshots(stream: ReadableStream<Uint8Array>, onSnapshot: (snapshot: TerminalSnapshot) => void) {
  let buffer = ""
  const reader = stream.getReader()
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })
      let newline = buffer.indexOf("\n")
      while (newline >= 0) {
        const line = buffer.slice(0, newline).trim()
        buffer = buffer.slice(newline + 1)
        if (line) emitSnapshot(line, onSnapshot)
        newline = buffer.indexOf("\n")
      }
    }
  } finally {
    reader.releaseLock()
  }
}

function emitSnapshot(line: string, onSnapshot: (snapshot: TerminalSnapshot) => void) {
  try {
    const parsed = JSON.parse(line) as { type?: string } & TerminalSnapshot
    if (parsed.type === "snapshot") onSnapshot(parsed)
  } catch {
    // ignore non-JSON noise
  }
}
