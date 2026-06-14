import type { PermissionDecision, SessionEvent } from "./backend"
import type { StoredSessionDetail } from "./backend"
import { createUnifiedPatchFromContent } from "./diff_patch"

export type TranscriptItem =
  | { id: string; kind: "user"; text: string }
  | { id: string; kind: "assistant"; text: string }
  | {
      id: string
      kind: "tool"
      toolCallId: string
      name: string
      input: string
      output?: string
      status: "running" | "done" | "error"
      approval?: PermissionDecision
      diff?: string
    }
  | { id: string; kind: "status"; text: string }
  | { id: string; kind: "error"; text: string }

export type PermissionRequest = {
  requestId: string
  toolName: string
  reason: string
  input: string
  filepath?: string
  diff?: string
}

type PermissionApproval = {
  toolName: string
  filepath?: string
  decision: PermissionDecision
}

export type ModifiedFile = {
  file: string
  additions: number
  deletions: number
  diff?: string
}

export type AppState = {
  transcript: TranscriptItem[]
  pendingPermission?: PermissionRequest
  permissionApprovals: PermissionApproval[]
  modifiedFiles: ModifiedFile[]
  running: boolean
  status: string
  tokens: number
  costUsd: number
  title: string
}

export function createInitialState(): AppState {
  return {
    transcript: [],
    permissionApprovals: [],
    modifiedFiles: [],
    running: false,
    status: "idle",
    tokens: 0,
    costUsd: 0,
    title: "New session",
  }
}

export function addUserMessage(state: AppState, text: string): AppState {
  return {
    ...state,
    running: true,
    title: state.title === "New session" ? summarizeTitle(text) : state.title,
    transcript: [...state.transcript, { id: nextId("user"), kind: "user", text }],
  }
}

export function loadStoredSession(detail: StoredSessionDetail): AppState {
  const title = detail.session.display_name || summarizeTitle(firstUserMessage(detail) ?? "")
  const initial = {
    ...createInitialState(),
    title,
    status: String(detail.session.status ?? "idle").toLowerCase(),
  }
  if (Array.isArray(detail.events) && detail.events.length > 0) {
    const firstPrompt = firstUserMessage(detail)
    const withPrompt = firstPrompt
      ? { ...initial, transcript: [{ id: nextId("user"), kind: "user" as const, text: firstPrompt }] }
      : initial
    return detail.events.reduce((current, event) => applySessionEvent(current, event), withPrompt)
  }
  const transcript = detail.messages
    .map(storedMessageToTranscriptItem)
    .filter((item): item is TranscriptItem => Boolean(item))
  return {
    ...initial,
    transcript,
  }
}

export function applySessionEvent(state: AppState, event: SessionEvent): AppState {
  switch (event.type) {
    case "status":
      return { ...state, status: String(event.status ?? "unknown") }
    case "text_delta":
      return appendAssistantText(state, event.text ?? "")
    case "text_start":
    case "text_end":
    case "reasoning_start":
    case "reasoning_end":
    case "tool_input_start":
    case "tool_input_delta":
    case "tool_input_end":
      return state
    case "reasoning_delta":
      return appendAssistantText(state, event.text ?? event.delta ?? "")
    case "tool_call_requested":
      return appendRequestedTool(state, event.name ?? "tool", event.input_json, event.tool_call_id ? String(event.tool_call_id) : undefined)
    case "permission_resolved":
      return { ...state, pendingPermission: undefined, status: "running_tools" }
    case "context_prepared":
      return { ...state, tokens: Number(event.token_count ?? state.tokens) }
    case "step_start":
    case "step_finish":
      return state
    case "tool_call_start":
      {
        const input = stringify(event.input_json)
        const approval = findMatchingApproval(state.permissionApprovals, event.name ?? "tool", input)
        const permissionApprovals = approval
          ? state.permissionApprovals.filter((item) => item !== approval)
          : state.permissionApprovals
        return {
          ...state,
          permissionApprovals,
          modifiedFiles: isMutatingTool(event.name ?? "tool")
            ? mergeModifiedFiles(state.modifiedFiles, modifiedFileFromInput(event.input_json))
            : state.modifiedFiles,
          transcript: upsertStartedTool(state.transcript, event.name ?? "tool", input, String(event.tool_call_id ?? nextId("call")), approval?.decision),
        }
      }
    case "tool_call_progress":
      return appendToolOutput(state, String(event.tool_call_id ?? ""), `${event.message ?? ""}\n`, "running")
    case "tool_call_result":
      return appendToolOutput(state, String(event.tool_call_id ?? ""), event.output ?? "", "done")
    case "tool_call_error":
      return appendToolOutput(state, String(event.tool_call_id ?? ""), event.message ?? "", "error")
    case "patch":
      return {
        ...state,
        modifiedFiles: mergePatchFiles(state.modifiedFiles, event.files),
        transcript: attachPatchToLatestMutatingTool(state.transcript, event.files),
      }
    case "diagnostics":
      return appendDiagnostics(state, event.files)
    case "permission_request": {
      const preview = permissionPreview(event.input_json)
      return {
        ...state,
        status: "waiting_for_permission",
        pendingPermission: {
          requestId: String(event.request_id ?? ""),
          toolName: event.tool_name ?? "tool",
          reason: event.reason ?? "approval required",
          input: stringify(event.input_json),
          filepath: preview.file?.file,
          diff: preview.diff,
        },
      }
    }
    case "usage":
      return {
        ...state,
        tokens: Number(event.input_tokens ?? 0) + Number(event.output_tokens ?? 0) + Number(event.cache_read_tokens ?? 0),
        costUsd: Number(event.total_cost_usd ?? state.costUsd),
      }
    case "terminal_output":
      return appendTerminalOutput(state, event.chunk ?? "")
    case "result":
      if (event.stop_reason === "interrupted") return markAgentStopped(state)
      return { ...state, running: false, status: String(event.stop_reason ?? "completed"), pendingPermission: undefined }
    case "error":
      return {
        ...state,
        running: false,
        transcript: [...state.transcript, { id: nextId("error"), kind: "error", text: event.message ?? "unknown error" }],
      }
    default:
      return state
  }
}

export function markAgentStopped(state: AppState): AppState {
  return {
    ...state,
    running: false,
    status: "stopped",
    pendingPermission: undefined,
    transcript: appendStoppedAgent(state.transcript),
  }
}

function appendStoppedAgent(transcript: TranscriptItem[]): TranscriptItem[] {
  const last = transcript.at(-1)
  if (last?.kind === "error" && last.text === "stopped agent") return transcript
  return [...transcript, { id: nextId("error"), kind: "error", text: "stopped agent" }]
}

function mergePatchFiles(current: ModifiedFile[], files: SessionEvent["files"]): ModifiedFile[] {
  if (!Array.isArray(files)) return current
  return files.reduce((merged, file) => {
    if (!file?.path) return merged
    return mergeModifiedFiles(merged, {
      file: file.path,
      additions: Number(file.additions ?? 0),
      deletions: Number(file.deletions ?? 0),
      diff: typeof file.diff === "string" ? file.diff : undefined,
    })
  }, current)
}

function attachPatchToLatestMutatingTool(transcript: TranscriptItem[], files: SessionEvent["files"]): TranscriptItem[] {
  const diff = patchEventDiff(files)
  if (!diff) return transcript
  for (let index = transcript.length - 1; index >= 0; index -= 1) {
    const item = transcript[index]
    if (item.kind !== "tool" || !isMutatingTool(item.name)) continue
    return transcript.map((candidate, candidateIndex) => {
      if (candidateIndex !== index || candidate.kind !== "tool") return candidate
      return { ...candidate, diff }
    })
  }
  return transcript
}

function patchEventDiff(files: SessionEvent["files"]) {
  if (!Array.isArray(files)) return undefined
  const diffs = files
    .map((file) => (typeof file?.diff === "string" ? file.diff : undefined))
    .filter((diff): diff is string => Boolean(diff?.trim()))
  if (diffs.length === 0) return undefined
  return diffs.join("\n")
}

export function applyPermissionDecision(state: AppState, decision: PermissionDecision): AppState {
  const transcript = markPermissionDecision(state.transcript, state.pendingPermission, decision)
  const matchedExistingTool = transcript !== state.transcript
  const approval = state.pendingPermission
    ? { toolName: state.pendingPermission.toolName, filepath: state.pendingPermission.filepath, decision }
    : undefined
  return {
    ...state,
    pendingPermission: undefined,
    permissionApprovals: matchedExistingTool || !approval ? state.permissionApprovals : [...state.permissionApprovals, approval],
    transcript,
  }
}

function appendAssistantText(state: AppState, text: string): AppState {
  if (!text) return state
  const transcript = [...state.transcript]
  const last = transcript[transcript.length - 1]
  if (last?.kind === "assistant") {
    transcript[transcript.length - 1] = { ...last, text: last.text + text }
  } else {
    transcript.push({ id: nextId("assistant"), kind: "assistant", text })
  }
  return { ...state, transcript }
}

function appendRequestedTool(state: AppState, name: string, inputJson: unknown, toolCallId?: string): AppState {
  const input = stringify(inputJson)
  if (hasMatchingRequestedTool(state.transcript, name, input, toolCallId)) return state
  return {
    ...state,
    transcript: [
      ...state.transcript,
      {
        id: nextId("tool"),
        kind: "tool",
        toolCallId: toolCallId ?? nextId("requested"),
        name,
        input,
        status: "running",
      },
    ],
  }
}

function hasMatchingRequestedTool(transcript: TranscriptItem[], name: string, input: string, toolCallId?: string) {
  return transcript.some((item) => {
    if (item.kind !== "tool" || item.status !== "running") return false
    if (toolCallId && item.toolCallId === toolCallId) return true
    return item.name === name && item.input === input
  })
}

function upsertStartedTool(
  transcript: TranscriptItem[],
  name: string,
  input: string,
  toolCallId: string,
  approval?: PermissionDecision,
): TranscriptItem[] {
  const index = transcript.findIndex((item) => {
    if (item.kind !== "tool" || item.status !== "running") return false
    if (item.toolCallId === toolCallId) return true
    return item.name === name && item.input === input
  })
  if (index < 0) {
    return [
      ...transcript,
      {
        id: nextId("tool"),
        kind: "tool",
        toolCallId,
        name,
        input,
        status: "running",
        approval,
      },
    ]
  }
  return transcript.map((item, itemIndex) => {
    if (itemIndex !== index || item.kind !== "tool") return item
    return { ...item, toolCallId, name, input, status: "running", approval: approval ?? item.approval }
  })
}

function appendToolOutput(state: AppState, toolCallId: string, output: string, status: "running" | "done" | "error"): AppState {
  const transcript = state.transcript.map((item) => {
    if (item.kind !== "tool" || item.toolCallId !== toolCallId) return item
    return { ...item, output: `${item.output ?? ""}${output}`, status }
  })
  return { ...state, transcript }
}

function appendDiagnostics(state: AppState, diagnostics: SessionEvent["files"]): AppState {
  if (!Array.isArray(diagnostics) || diagnostics.length === 0) return state
  return state
}

function appendTerminalOutput(state: AppState, chunk: string): AppState {
  if (!chunk) return state
  let next = state
  const lines = chunk.split(/(\n)/)
  let buffer = ""
  for (const part of lines) {
    if (part === "\n") {
      const parsed = parseRequestedToolText(buffer.trim())
      next = parsed
        ? appendRequestedTool(next, parsed.name, parsed.input, parsed.toolCallId)
        : appendAssistantText(next, `${buffer}\n`)
      buffer = ""
      continue
    }
    buffer += part
  }
  if (buffer) {
    const parsed = parseRequestedToolText(buffer.trim())
    next = parsed
      ? appendRequestedTool(next, parsed.name, parsed.input, parsed.toolCallId)
      : appendAssistantText(next, buffer)
  }
  return next
}

function stringify(value: unknown): string {
  if (value === undefined || value === null) return ""
  if (typeof value === "string") return value
  return JSON.stringify(value, null, 2)
}

function summarizeTitle(text: string) {
  const clean = text.replace(/\s+/g, " ").trim()
  if (!clean) return "New session"
  return clean.length > 34 ? `${clean.slice(0, 31)}...` : clean
}

function firstUserMessage(detail: StoredSessionDetail) {
  return detail.messages.find((message) => message.role.toLowerCase() === "user")?.content
}

function storedMessageToTranscriptItem(message: StoredSessionDetail["messages"][number]): TranscriptItem | undefined {
  const role = message.role.toLowerCase()
  const requestedTool = parseRequestedToolText(message.content.trim())
  if (requestedTool) {
    return {
      id: nextId("tool"),
      kind: "tool",
      toolCallId: requestedTool.toolCallId ?? nextId("stored-requested"),
      name: requestedTool.name,
      input: stringify(requestedTool.input),
      status: "done",
    }
  }
  if (role === "user") return { id: nextId("user"), kind: "user", text: message.content }
  if (role === "assistant") return { id: nextId("assistant"), kind: "assistant", text: message.content }
  if (role === "tool") return storedToolMessageToTranscriptItem(message.content)
  return undefined
}

function storedToolMessageToTranscriptItem(content: string): TranscriptItem | undefined {
  const text = content.trim()
  if (!text || text.startsWith("Tool:") || text.startsWith("tool call error")) return undefined
  const resultMatch = text.match(/^([a-zA-Z0-9_-]+)\s+result:\s*\n?([\s\S]*)$/)
  if (resultMatch) {
    return {
      id: nextId("tool"),
      kind: "tool",
      toolCallId: nextId("stored-tool"),
      name: resultMatch[1],
      input: "",
      output: resultMatch[2] ?? "",
      status: "done",
    }
  }
  return undefined
}

function parseRequestedToolText(text: string): { name: string; input: unknown; toolCallId?: string } | undefined {
  const prefix = "tool call requested:"
  if (!text.toLowerCase().startsWith(prefix)) return undefined
  const raw = text.slice(prefix.length).trim()
  if (!raw.startsWith("{")) return undefined
  try {
    const parsed = JSON.parse(raw) as Record<string, unknown>
    const name = typeof parsed.name === "string" && parsed.name ? parsed.name : "tool"
    const input = "input" in parsed ? parsed.input : parsed.input_json
    const toolCallId = typeof parsed.tool_call_id === "string" ? parsed.tool_call_id : undefined
    return { name, input, toolCallId }
  } catch {
    return undefined
  }
}

function permissionPreview(value: unknown): { file?: ModifiedFile; diff?: string } {
  if (!value || typeof value !== "object") return {}
  const record = value as Record<string, unknown>
  const diff = typeof record.diff === "string" ? record.diff : typeof record.patch === "string" ? record.patch : undefined
  const path =
    stringField(record, "path") ??
    stringField(record, "filepath") ??
    stringField(record, "file_path") ??
    stringField(record, "target")
  if (!path) return {}
  if (diff) {
    const file = modifiedFileFromDiff(path, diff)
    return { file, diff: file.diff }
  }
  const file = modifiedFileFromInput(value)
  return { file, diff: file?.diff }
}

function modifiedFileFromInput(value: unknown): ModifiedFile | undefined {
  if (!value || typeof value !== "object") return undefined
  const record = value as Record<string, unknown>
  const path =
    stringField(record, "path") ??
    stringField(record, "filepath") ??
    stringField(record, "file_path") ??
    stringField(record, "target")
  if (!path) return undefined
  const directDiff = typeof record.diff === "string" ? record.diff : typeof record.patch === "string" ? record.patch : undefined
  const oldText = stringField(record, "old") ?? stringField(record, "old_text") ?? stringField(record, "before")
  const newText = stringField(record, "new") ?? stringField(record, "new_text") ?? stringField(record, "content") ?? stringField(record, "after")
  const diff = directDiff ?? (oldText || newText ? createUnifiedPatchFromContent(path, oldText ?? "", newText ?? "") : undefined)
  return modifiedFileFromDiff(path, diff)
}

function modifiedFileFromDiff(file: string, diff?: string): ModifiedFile {
  if (!diff) return { file, additions: 0, deletions: 0 }
  let additions = 0
  let deletions = 0
  for (const line of diff.split("\n")) {
    if (line.startsWith("+++") || line.startsWith("---")) continue
    if (line.startsWith("+")) additions += 1
    if (line.startsWith("-")) deletions += 1
  }
  return { file, additions, deletions, diff }
}

function mergeModifiedFiles(current: ModifiedFile[], next: ModifiedFile | undefined): ModifiedFile[] {
  if (!next) return current
  const index = current.findIndex((item) => item.file === next.file)
  if (index < 0) return [...current, next]
  const merged = [...current]
  merged[index] = {
    file: next.file,
    additions: Math.max(merged[index].additions, next.additions),
    deletions: Math.max(merged[index].deletions, next.deletions),
    diff: next.diff ?? merged[index].diff,
  }
  return merged
}

function markPermissionDecision(
  transcript: TranscriptItem[],
  request: PermissionRequest | undefined,
  decision: PermissionDecision,
): TranscriptItem[] {
  if (!request) return transcript
  const index = findPermissionToolIndex(transcript, request)
  if (index < 0) return transcript
  return transcript.map((item, itemIndex) => {
    if (itemIndex !== index || item.kind !== "tool") return item
    return { ...item, approval: decision }
  })
}

function findPermissionToolIndex(transcript: TranscriptItem[], request: PermissionRequest): number {
  for (let index = transcript.length - 1; index >= 0; index -= 1) {
    const item = transcript[index]
    if (item.kind !== "tool") continue
    if (item.name === request.toolName) return index
    if (request.filepath && item.input.includes(request.filepath)) return index
  }
  return -1
}

function findMatchingApproval(approvals: PermissionApproval[], toolName: string, input: string) {
  for (let index = approvals.length - 1; index >= 0; index -= 1) {
    const approval = approvals[index]
    if (approval.toolName !== toolName) continue
    if (!approval.filepath || input.includes(approval.filepath)) return approval
  }
  return undefined
}

function isMutatingTool(name: string) {
  const lower = name.toLowerCase()
  return lower === "write_file" || lower === "edit_file" || lower === "multi_edit" || lower.startsWith("apply_patch") || lower.includes("edit")
}

function stringField(record: Record<string, unknown>, key: string): string | undefined {
  const value = record[key]
  return typeof value === "string" && value.length > 0 ? value : undefined
}

let id = 0
function nextId(prefix: string) {
  id += 1
  return `${prefix}-${id}`
}
