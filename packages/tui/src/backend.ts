export type PermissionDecision = "allow" | "allow_always" | "deny"

export type SessionEvent = {
  type: string
  session_id?: string
  status?: string
  text?: string
  stop_reason?: string
  message?: string
  name?: string
  tool_call_id?: string
  input_json?: unknown
  output?: string
  exit_code?: number | null
  request_id?: string
  reason?: string
  tool_name?: string
  chunk?: string
  input_tokens?: number | null
  output_tokens?: number | null
  cache_read_tokens?: number | null
  total_cost_usd?: number | null
  token_count?: number
  original_token_count?: number
  compacted?: boolean
  summary?: string | null
  index?: number
  files?: Array<{ path?: string; additions?: number; deletions?: number; diff?: string | null; exists?: boolean; bytes?: number | null; lines?: number | null }>
  additions?: number
  deletions?: number
  text_id?: string
  reasoning_id?: string
  delta?: string
  decision?: PermissionDecision
}

export type DevMode = "in-place" | "worktree"

export type BackendOptions = {
  backendBin: string
  workspace: string
  provider: string
  model?: string
  sessionId?: string
  effort?: string
  approval: string
  repoRoot: string
  appDb?: string
  /** Development mode: edit in place, or run inside an isolated git worktree. */
  mode?: DevMode
  /** Override the path the run reads/writes session state from (state.db). */
  stateDb?: string
}

export type Worktree = {
  workspace_id: string
  source_repo: string
  worktree_path: string
  state_db?: string | null
  branch_name: string
  base_branch: string
  status: "active" | "merged" | "abandoned" | "archived"
  exists: boolean
  display_name?: string | null
  session_id?: string | null
  session_status?: string | null
  provider?: string | null
  model?: string | null
  updated_at: string
}

export type MergeResult =
  | { result: "up_to_date"; target: string }
  | { result: "merged"; commit: string; fast_forward: boolean; target: string }
  | { result: "conflict"; target: string; source_repo: string; files: string[] }

export type BackendCallbacks = {
  onEvent(event: SessionEvent): void
  onStderr(text: string): void
  onExit(code: number | null): void
}

export type BackendRun = {
  respond(requestId: string, decision: PermissionDecision, message?: string): void
  interrupt(): void
  kill(): void
}

export type StoredSession = {
  id: string
  provider: string
  model: string
  status: string
  display_name?: string | null
  created_at: string
  updated_at: string
  preview: string
}

export type StoredMessage = {
  role: string
  content: string
  ordinal: number
}

export type StoredSessionDetail = {
  session: {
    id: string
    provider_id?: { 0?: string } | string
    model: string
    status: string
    display_name?: string | null
    created_at: string
    updated_at: string
  }
  messages: StoredMessage[]
  events?: SessionEvent[]
}

const decoder = new TextDecoder()
const encoder = new TextEncoder()

export function startBackendTurn(prompt: string, options: BackendOptions, callbacks: BackendCallbacks): BackendRun {
  const cmd = [
    options.backendBin,
    "run",
    "--provider",
    options.provider,
    "--workspace",
    options.workspace,
    "--prompt",
    prompt,
    "--approval",
    options.approval,
  ]

  if (options.model) {
    cmd.push("--model", options.model)
  }
  if (options.sessionId) {
    cmd.push("--session-id", options.sessionId)
  }
  if (options.effort) {
    cmd.push("--effort", options.effort)
  }
  if (options.mode) {
    cmd.push("--mode", options.mode)
  }
  if (options.appDb) {
    cmd.push("--app-db", options.appDb)
  }
  if (options.stateDb) {
    cmd.push("--state-db", options.stateDb)
  }

  const proc = Bun.spawn(cmd, {
    cwd: options.repoRoot,
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  })

  void readJsonLines(proc.stdout, callbacks)
  void readStderr(proc.stderr, callbacks.onStderr)
  void proc.exited.then((code) => callbacks.onExit(code))

  return {
    respond(requestId, decision, message) {
      const line = JSON.stringify({
        type: "permission_decision",
        request_id: requestId,
        decision,
        ...(message ? { message } : {}),
      })
      proc.stdin.write(encoder.encode(`${line}\n`))
    },
    interrupt() {
      proc.kill("SIGINT")
    },
    kill() {
      proc.kill()
    },
  }
}

export async function listWorkspaceSessions(options: Pick<BackendOptions, "backendBin" | "repoRoot" | "workspace">): Promise<StoredSession[]> {
  const output = await runBackendJson(options, [
    "db",
    "sessions",
    "--workspace",
    options.workspace,
    "--json",
  ])
  return Array.isArray(output) ? output as StoredSession[] : []
}

export async function showWorkspaceSession(options: Pick<BackendOptions, "backendBin" | "repoRoot" | "workspace">, sessionId: string, stateDb?: string): Promise<StoredSessionDetail> {
  const args = [
    "db",
    "show-session",
    "--workspace",
    options.workspace,
    "--session-id",
    sessionId,
    "--json",
  ]
  if (stateDb) args.push("--state-db", stateDb)
  const output = await runBackendJson(options, args)
  return output as StoredSessionDetail
}

export async function listWorktrees(options: Pick<BackendOptions, "backendBin" | "repoRoot" | "appDb">): Promise<Worktree[]> {
  const args = ["worktree", "registry", "--json"]
  if (options.appDb) args.push("--app-db", options.appDb)
  const output = await runBackendJson(options, args)
  return Array.isArray(output) ? (output as Worktree[]) : []
}

export async function mergeWorktree(
  options: Pick<BackendOptions, "backendBin" | "repoRoot" | "appDb">,
  workspaceId: string,
): Promise<MergeResult> {
  const args = ["worktree", "merge", "--workspace-id", workspaceId, "--json"]
  if (options.appDb) args.push("--app-db", options.appDb)
  return (await runBackendJson(options, args)) as MergeResult
}

export async function archiveWorktree(
  options: Pick<BackendOptions, "backendBin" | "repoRoot" | "appDb">,
  workspaceId: string,
): Promise<void> {
  const args = ["worktree", "archive", "--workspace-id", workspaceId, "--json"]
  if (options.appDb) args.push("--app-db", options.appDb)
  await runBackendJson(options, args)
}

export async function abortWorktreeMerge(
  options: Pick<BackendOptions, "backendBin" | "repoRoot" | "appDb">,
  workspaceId: string,
): Promise<void> {
  const args = ["worktree", "abort-merge", "--workspace-id", workspaceId]
  if (options.appDb) args.push("--app-db", options.appDb)
  // abort-merge prints a human line, not JSON; just run it.
  await runBackendText(options, args)
}

async function runBackendText(options: Pick<BackendOptions, "backendBin" | "repoRoot">, args: string[]): Promise<string> {
  const proc = Bun.spawn([options.backendBin, ...args], {
    cwd: options.repoRoot,
    stdout: "pipe",
    stderr: "pipe",
  })
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ])
  if (code !== 0) {
    throw new Error(stderr.trim() || `backend exited ${code}`)
  }
  return stdout
}

async function runBackendJson(options: Pick<BackendOptions, "backendBin" | "repoRoot">, args: string[]): Promise<unknown> {
  const proc = Bun.spawn([options.backendBin, ...args], {
    cwd: options.repoRoot,
    stdout: "pipe",
    stderr: "pipe",
  })
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ])
  if (code !== 0) {
    throw new Error(stderr.trim() || `backend exited ${code}`)
  }
  return JSON.parse(stdout)
}

async function readJsonLines(stream: ReadableStream<Uint8Array>, callbacks: BackendCallbacks) {
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
        if (line) emitLine(line, callbacks)
        newline = buffer.indexOf("\n")
      }
    }
  } finally {
    reader.releaseLock()
  }
  const tail = buffer.trim()
  if (tail) emitLine(tail, callbacks)
}

async function readStderr(stream: ReadableStream<Uint8Array>, onStderr: (text: string) => void) {
  const reader = stream.getReader()
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      const text = decoder.decode(value, { stream: true })
      if (text) onStderr(text)
    }
  } finally {
    reader.releaseLock()
  }
}

function emitLine(line: string, callbacks: BackendCallbacks) {
  try {
    callbacks.onEvent(JSON.parse(line) as SessionEvent)
  } catch {
    callbacks.onStderr(`${line}\n`)
  }
}
