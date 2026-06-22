/** @jsxImportSource @opentui/solid */
import { BoxRenderable, MacOSScrollAccel, SyntaxStyle, TextAttributes, TextareaRenderable, type KeyEvent } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { execFile } from "node:child_process"
import { promisify } from "node:util"
import path from "node:path"
import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js"
import { createStore, produce } from "solid-js/store"
import {
  applyPermissionDecision,
  applySessionEvent,
  addUserMessage,
  createInitialState,
  loadStoredSession,
  markAgentStopped,
  type AppState,
  type ModifiedFile,
  type TranscriptItem,
} from "./state"
import { archiveWorktree, listProviderModels, listWorktrees, showWorkspaceSession, startBackendTurn, startCopilotLogin, type AuthStatusEvent, type BackendOptions, type BackendRun, type DevMode, type PermissionDecision, type ProviderModel, type Worktree } from "./backend"
import { readClipboard } from "./clipboard"
import { createUnifiedPatchFromContent, normalizeDiffForRendering, normalizeUnifiedPatch, patchFilesFromUnifiedPatch } from "./diff_patch"
import { openExternalDiffViewer } from "./diff_viewer"
import {
  appendPromptToken,
  findActiveMention,
  isImagePath,
  listFileChoices,
  pastedImageName,
  promptForSubmit,
  replaceMention,
  stripToken,
  toWorkspacePath,
  type FileChoice,
  type MentionState,
  type PromptImageAttachment,
} from "./mentions"
import { insertTextAtCursor, parsePromptHistory, recordPromptHistory, serializePromptHistory, shouldNavigateHistory, stepPromptHistory, type HistoryDirection, type PromptHistoryState } from "./prompt_input"
import { spawnTerminalSession, type TerminalSession, type TerminalSnapshot } from "./terminal"

export type AppProps = BackendOptions & {
  exitApp(): void
  registerCtrlCHandler(handler: (() => void) | undefined): void
}

type EffortValue = "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultracode"

/** One concurrent agent: its session, worktree, provider/model and transcript. */
type AgentSlot = {
  key: string
  sessionId?: string
  workspaceId?: string
  branch: string
  provider: string
  model: string
  effort: EffortValue
  devMode: DevMode
  approval: string
  workspaceOnly: boolean
  role: string
  stateDb?: string
  state: AppState
}

/** Ephemeral per-run bookkeeping for a slot's live subprocess. */
type RunFlags = { stopping: boolean; exitAfter: boolean; forceTimer?: ReturnType<typeof setTimeout> }
type PaletteKind = "commands" | "models" | "connect" | "agents" | "modes" | "permissions" | "files" | undefined
type CommandAction = "agents" | "clear" | "connect" | "exit" | "help" | "mode" | "model" | "new" | "permissions" | "pr" | "review" | "sessions"
type Command = { name: string; description: string; action: CommandAction }
type ModelChoice = { provider: string; model: string; label: string; group: string; effortName: string; efforts: EffortValue[]; effortLabels?: Partial<Record<EffortValue, string>> }
type ConnectChoice = { provider: string; label: string; description: string }
type EffortChoice = { name: string; label: string; description: string; value: EffortValue }
type AgentChoice = { name: string; description: string }
type PermissionChoice = { name: string; label: string; description: string; approval: string; workspaceOnly: boolean }
type PaletteItem = Command | ModelChoice | ConnectChoice | EffortChoice | AgentChoice | PermissionChoice | FileChoice
type StopIntent = "interrupt" | "exit"
type PrFlow = { step: "base" | "message"; base: string; worktree: Worktree }
type NoticeTone = "cyan" | "red" | "muted"
type ComposerNotice = { text: string; tone: NoticeTone }
const permissionActions = ["allow", "allow_always", "deny"] as const

const theme = {
  bg: "transparent",
  surface: "transparent",
  surface2: "transparent",
  surface3: "transparent",
  panel: "transparent",
  panelSoft: "transparent",
  row: "transparent",
  text: "#e8e8e8",
  muted: "#8b9298",
  dim: "#5a6268",
  faint: "#202223",
  border: "#34383b",
  borderSoft: "#24282b",
  borderStrong: "#495057",
  rail: "#5a6670",
  railActive: "#1eb9ff",
  blue: "#2cb9ff",
  cyan: "#22d3ff",
  green: "#7ee787",
  red: "#ff5c7a",
  yellow: "#f2b86b",
  purple: "#b18cff",
  orange: "#ffbf80",
  addedBg: "#142f25",
  removedBg: "#34191b",
  selectionBg: "#3b4d6c",
  palette: "transparent",
  paletteSelected: "#12303a",
  progress: "#24c8ff",
  progressTrack: "transparent",
}

const SESSION_SIDEBAR_WIDTH = 46
const SESSION_SIDEBAR_TEXT_WIDTH = SESSION_SIDEBAR_WIDTH - 6
const TELEMETRY_SIDEBAR_WIDTH = 32
const TELEMETRY_PROGRESS_WIDTH = 24
const TELEMETRY_FILE_WIDTH = 22
const TELEMETRY_FOOTER_WIDTH = 24
const commands: Command[] = [
  { name: "/agents", description: "Switch agent", action: "agents" },
  { name: "/connect", description: "Connect provider", action: "connect" },
  { name: "/effort", description: "Switch reasoning effort", action: "mode" },
  { name: "/fast", description: "Switch reasoning effort", action: "mode" },
  { name: "/help", description: "Show shortcuts", action: "help" },
  { name: "/model", description: "Switch model", action: "model" },
  { name: "/new", description: "New session", action: "new" },
  { name: "/permissions", description: "Switch agent permissions", action: "permissions" },
  { name: "/pr", description: "Create pull request", action: "pr" },
  { name: "/review", description: "Review changes", action: "review" },
  { name: "/sessions", description: "Open sessions", action: "sessions" },
  { name: "/clear", description: "Clear transcript", action: "clear" },
  { name: "/exit", description: "Exit app", action: "exit" },
]

let modelChoices: ModelChoice[] = [
  { group: "Claude", provider: "claude", model: "sonnet", label: "Claude Sonnet", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "Claude", provider: "claude", model: "fable", label: "Fable", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "Claude", provider: "claude", model: "opus", label: "Opus (1M context)", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "Claude", provider: "claude", model: "haiku", label: "Haiku", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "OpenAI", provider: "codex", model: "gpt-5.5", label: "GPT-5.5", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.4", label: "GPT-5.4", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.4-mini", label: "GPT-5.4-Mini", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "GitHub Copilot", provider: "copilot", model: "gpt-4.1", label: "Copilot GPT-4.1", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"] },
  { group: "GitHub Copilot", provider: "copilot", model: "claude-sonnet-4", label: "Copilot Claude Sonnet 4", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"] },
  { group: "GitHub Copilot", provider: "copilot", model: "o4-mini", label: "Copilot o4-mini", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"] },
]

const connectChoices: ConnectChoice[] = [
  { provider: "claude", label: "Claude", description: "Use Claude Code credentials" },
  { provider: "codex", label: "OpenAI", description: "Use Codex auth.json credentials" },
  { provider: "copilot", label: "GitHub Copilot", description: "Start GitHub device login" },
]

const agentChoices: AgentChoice[] = [
  { name: "Build", description: "Implement and verify changes" },
  { name: "Review", description: "Inspect code and risks first" },
  { name: "Plan", description: "Outline before editing" },
]

const permissionChoices: PermissionChoice[] = [
  { name: "yolo", label: "Yolo", description: "Run reads, edits, and bash without prompts", approval: "never", workspaceOnly: false },
  { name: "workspace", label: "Workspace Only", description: "No prompts, but file/bash access stays in workspace", approval: "never", workspaceOnly: true },
  { name: "mutating", label: "Ask Mutating", description: "Ask before file edits, writes, patches, and bash", approval: "mutating", workspaceOnly: false },
  { name: "risky", label: "Ask Risky", description: "Ask only for risk-flagged actions", approval: "on-request", workspaceOnly: false },
  { name: "always", label: "Ask Every Tool", description: "Ask before every tool call", approval: "always", workspaceOnly: false },
]

const money = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" })
const scrollAcceleration = new MacOSScrollAccel({ maxMultiplier: 2.8 })
const execFileAsync = promisify(execFile)
const syntaxStyle = SyntaxStyle.fromTheme([
  { scope: ["keyword", "storage.type", "keyword.control"], style: { foreground: theme.red } },
  { scope: ["string", "punctuation.definition.string"], style: { foreground: theme.green } },
  { scope: ["function", "entity.name.function"], style: { foreground: theme.purple, italic: true } },
  { scope: ["variable", "property"], style: { foreground: theme.text } },
  { scope: ["comment"], style: { foreground: theme.dim, italic: true } },
  { scope: ["constant", "number"], style: { foreground: theme.yellow } },
  { scope: ["diff.plus"], style: { foreground: theme.green, background: theme.addedBg } },
  { scope: ["diff.minus"], style: { foreground: theme.red, background: theme.removedBg } },
  { scope: ["markup.heading", "heading"], style: { foreground: theme.text, bold: true } },
  { scope: ["markup.bold"], style: { foreground: theme.text, bold: true } },
  { scope: ["markup.italic"], style: { foreground: theme.yellow, italic: true } },
  { scope: ["markup.raw", "code"], style: { foreground: theme.green } },
])

function loadPromptHistoryFile(filePath: string): string[] {
  try {
    if (!existsSync(filePath)) return []
    return parsePromptHistory(readFileSync(filePath, "utf8"))
  } catch {
    return []
  }
}

function savePromptHistoryFile(filePath: string, entries: string[]) {
  try {
    mkdirSync(path.dirname(filePath), { recursive: true })
    writeFileSync(filePath, serializePromptHistory(entries))
  } catch {
    // history persistence is best-effort; ignore write failures
  }
}

export function App(props: AppProps) {
  let input!: TextareaRenderable
  let replacingPrompt = false
  let lastCtrlCAt = 0
  let stopArmTimer: ReturnType<typeof setTimeout> | undefined
  const startedAt = Date.now()

  // Each concurrent agent is a slot: its own session id, worktree, provider/
  // model, and AppState transcript. `runs`/`runFlags` hold the live subprocess
  // handles (non-reactive) keyed by slot. N agents run at once because each
  // backend turn's event callbacks are closed over its slot key.
  let agentSeq = 0
  function makeAgentSlot(init: Partial<AgentSlot> = {}): AgentSlot {
    agentSeq += 1
    return {
      key: `agent-${agentSeq}`,
      sessionId: init.sessionId,
      workspaceId: init.workspaceId,
      branch: init.branch ?? "HEAD",
      provider: init.provider ?? props.provider,
      model: init.model ?? props.model ?? defaultModel(props.provider),
      effort: init.effort ?? "medium",
      devMode: init.devMode ?? "worktree",
      approval: init.approval ?? props.approval,
      workspaceOnly: init.workspaceOnly ?? Boolean(props.workspaceOnly),
      role: init.role ?? "Build",
      stateDb: init.stateDb,
      state: init.state ?? createInitialState(),
    }
  }
  const initialAgent = makeAgentSlot()
  const [store, setStore] = createStore<{ agents: AgentSlot[]; focusedKey: string }>({
    agents: [initialAgent],
    focusedKey: initialAgent.key,
  })
  const runs = new Map<string, BackendRun>()
  const runFlags = new Map<string, RunFlags>()

  const focusedAgent = createMemo(() => store.agents.find((a) => a.key === store.focusedKey) ?? store.agents[0])
  const fstate = createMemo(() => focusedAgent().state)
  function agentIndex(key: string) {
    return store.agents.findIndex((a) => a.key === key)
  }
  function patchAgent(key: string, patch: Partial<AgentSlot>) {
    const idx = agentIndex(key)
    if (idx < 0) return
    setStore("agents", idx, patch)
  }
  function patchFocused(patch: Partial<AgentSlot>) {
    patchAgent(store.focusedKey, patch)
  }
  function updateAgentState(key: string, fn: (s: AppState) => AppState) {
    const idx = agentIndex(key)
    if (idx < 0) return
    setStore("agents", idx, "state", produce((s: AppState) => { Object.assign(s, fn(s)) }))
  }
  function runFlagsFor(key: string): RunFlags {
    let flags = runFlags.get(key)
    if (!flags) {
      flags = { stopping: false, exitAfter: false }
      runFlags.set(key, flags)
    }
    return flags
  }

  // Composer settings are views of the focused slot, so existing call sites
  // (provider(), setModel(x), ...) keep working while each agent owns its own.
  const provider = () => focusedAgent().provider
  const setProvider = (value: string) => patchFocused({ provider: value })
  const model = () => focusedAgent().model
  const setModel = (value: string) => patchFocused({ model: value })
  const mode = () => focusedAgent().effort
  const setMode = (value: EffortValue) => patchFocused({ effort: value })
  const devMode = () => focusedAgent().devMode
  const approval = () => focusedAgent().approval
  const setApproval = (value: string) => patchFocused({ approval: value })
  const workspaceOnly = () => focusedAgent().workspaceOnly
  const setWorkspaceOnly = (value: boolean) => patchFocused({ workspaceOnly: value })
  const agent = () => focusedAgent().role
  const setAgent = (value: string) => patchFocused({ role: value })
  const sessionId = () => focusedAgent().sessionId
  // The worktree (and its branch) is created on the first prompt, so derive the
  // branch from the matched worktree once it exists; until then the session has
  // no worktree of its own yet.
  const activeBranch = () => {
    const agent = focusedAgent()
    const worktree = worktrees().find((w) =>
      (agent.sessionId && agent.sessionId === w.session_id) ||
      (agent.workspaceId && agent.workspaceId === w.workspace_id)
    )
    if (worktree) return worktree.branch_name
    return agent.sessionId ? agent.branch : "new worktree"
  }

  const [draft, setDraft] = createSignal("")
  const [stopArmed, setStopArmed] = createSignal<StopIntent>()
  const [notice, setNotice] = createSignal<ComposerNotice>()
  const [copilotDeviceNotice, setCopilotDeviceNotice] = createSignal<ComposerNotice>()
  const [permissionSelected, setPermissionSelected] = createSignal(0)
  const promptHistoryPath = path.join(props.workspace, ".inductor", "prompt-history.json")
  const [promptHistory, setPromptHistory] = createSignal<PromptHistoryState>({ entries: loadPromptHistoryFile(promptHistoryPath), draft: "" })
  const [palette, setPalette] = createSignal<PaletteKind>()
  const [selected, setSelected] = createSignal(0)
  const [mention, setMention] = createSignal<MentionState>()
  const [pasteCount, setPasteCount] = createSignal(0)
  const [promptImages, setPromptImages] = createSignal<PromptImageAttachment[]>([])
  const [prFlow, setPrFlow] = createSignal<PrFlow>()
  const [worktrees, setWorktrees] = createSignal<Worktree[]>([])
  const [worktreeBusy, setWorktreeBusy] = createSignal<string>()
  const [sessionListStatus, setSessionListStatus] = createSignal("")
  const [expanded, setExpanded] = createSignal<Set<string>>(new Set())
  const [now, setNow] = createSignal(Date.now())
  const [modelCatalogVersion, setModelCatalogVersion] = createSignal(0)
  const dimensions = useTerminalDimensions()
  const contextPercent = createMemo(() => Math.min(99, Math.round((fstate().tokens / 200_000) * 100)))
  const hasTranscript = createMemo(() => fstate().transcript.length > 0 || fstate().running || Boolean(fstate().pendingPermission))
  // Full filesystem path the focused agent runs in: its managed worktree when
  // one exists, otherwise the workspace Inductor was opened in.
  const focusedWorktreePath = createMemo(() => {
    const agent = focusedAgent()
    const worktree = worktrees().find((w) =>
      (agent.sessionId && agent.sessionId === w.session_id) ||
      (agent.workspaceId && agent.workspaceId === w.workspace_id)
    )
    return worktree?.worktree_path ?? props.workspace
  })

  // Embedded terminal: a persistent shell scoped to the focused agent's
  // worktree path. The PTY lives in the backend; we stream screen snapshots in
  // and forward typed lines/control bytes out. It is recreated whenever the
  // focused worktree path changes so its cwd always matches the active agent.
  const TERMINAL_COLS = SESSION_SIDEBAR_TEXT_WIDTH
  const terminalRows = createMemo(() => Math.max(6, Math.floor((dimensions().height - 16) / 2)))
  const [terminalSnapshot, setTerminalSnapshot] = createSignal<TerminalSnapshot>()
  const [terminalError, setTerminalError] = createSignal<string>()
  let terminalSession: TerminalSession | undefined
  let terminalCwd: string | undefined
  function startTerminal(cwd: string) {
    terminalSession?.kill()
    setTerminalSnapshot(undefined)
    setTerminalError(undefined)
    terminalCwd = cwd
    let gotSnapshot = false
    terminalSession = spawnTerminalSession(props, cwd, { rows: terminalRows(), cols: TERMINAL_COLS }, {
      onSnapshot: (snapshot) => {
        gotSnapshot = true
        setTerminalSnapshot(snapshot)
      },
      onExit: () => {
        if (terminalCwd !== cwd) return
        terminalSession = undefined
        // Exiting before emitting any screen means the shell never started
        // (e.g. an outdated backend binary) — say so instead of hanging on
        // "starting shell…".
        if (!gotSnapshot) setTerminalError("shell unavailable — update the inductor backend")
      },
    })
  }
  function terminalWrite(data: string) {
    terminalSession?.write(data)
  }
  createEffect(() => {
    const cwd = focusedWorktreePath()
    if (cwd && cwd !== terminalCwd) startTerminal(cwd)
  })
  createEffect(() => {
    const rows = terminalRows()
    terminalSession?.resize({ rows, cols: TERMINAL_COLS })
  })

  const commandItems = createMemo(() => {
    const query = draft().trim()
    if (!query.startsWith("/")) return commands
    const filtered = commands.filter((command) => command.name.startsWith(query))
    return filtered.length > 0 ? filtered : commands
  })
  const fileItems = createMemo(() => {
    const active = mention()
    return active ? listFileChoices(props.workspace, active) : []
  })
  const paletteItems = createMemo(() => {
    if (palette() === "files") return fileItems()
    if (palette() === "models") {
      modelCatalogVersion()
      return modelChoices
    }
    if (palette() === "connect") return connectChoices
    if (palette() === "agents") return agentChoices
    if (palette() === "modes") return effortChoices(selectedModelChoice(provider(), model()))
    if (palette() === "permissions") return permissionChoices
    return commandItems()
  })

  const timer = setInterval(() => setNow(Date.now()), 120)
  const composerNotice = createMemo(() => {
    const state = fstate()
    return notice() ?? defaultComposerNotice(state.status, state.running, state.pendingPermission)
  })
  onMount(() => {
    props.registerCtrlCHandler(handleCtrlC)
    void refreshWorktrees()
    const worktreeRefreshTimer = setInterval(() => void refreshWorktrees(), 30_000)
    void refreshCopilotModels()
    onCleanup(() => clearInterval(worktreeRefreshTimer))
  })
  onCleanup(() => {
    props.registerCtrlCHandler(undefined)
    clearInterval(timer)
    clearStopArmTimer()
    for (const flags of runFlags.values()) {
      if (flags.forceTimer) clearTimeout(flags.forceTimer)
    }
    for (const run of runs.values()) run.kill()
    runs.clear()
    runFlags.clear()
    terminalSession?.kill()
  })

  useKeyboard((event) => {
    if (isCtrlC(event)) {
      const now = Date.now()
      if (event.eventType === "release" && now - lastCtrlCAt < 200) return
      lastCtrlCAt = now
      event.preventDefault()
      event.stopPropagation()
      handleCtrlC()
      return
    }
    if (event.eventType === "release") return
    if (event.repeated) return
    if (isNewSessionShortcut(event)) {
      event.preventDefault()
      event.stopPropagation()
      startNewSession()
      return
    }
    if (fstate().pendingPermission && handlePermissionKey(event)) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (isEscape(event) && prFlow()) {
      event.preventDefault()
      event.stopPropagation()
      setPrFlow(undefined)
      replacePrompt("")
      setNotice({ text: "PR creation cancelled", tone: "muted" })
      return
    }
    if (isEscape(event) && (fstate().running || runs.has(store.focusedKey))) {
      event.preventDefault()
      event.stopPropagation()
      handleEsc()
      return
    }
    disarmStopWarning()
  })

  function submit() {
    const visiblePrompt = input.plainText.trim()
    const prompt = promptForSubmit(visiblePrompt, promptImages()).trim()
    if (fstate().running) return
    if (palette()) {
      acceptPalette()
      return
    }
    if (prFlow()) {
      void submitPrFlow(visiblePrompt)
      return
    }
    if (!visiblePrompt) return

    // Pin the turn to the currently focused slot so its events route here even
    // after the user switches focus to another running agent.
    const key = store.focusedKey
    const flags = runFlagsFor(key)
    flags.stopping = false
    flags.exitAfter = false
    if (flags.forceTimer) {
      clearTimeout(flags.forceTimer)
      flags.forceTimer = undefined
    }

    input.setText("")
    setDraft("")
    recordHistory(visiblePrompt)
    setPromptImages([])
    setPalette(undefined)
    disarmStopWarning()
    setNotice(undefined)
    updateAgentState(key, (next) => addUserMessage(next, visiblePrompt))
    const run = startBackendTurn(prompt, {
      ...props,
      provider: provider(),
      model: model(),
      approval: approval(),
      workspaceOnly: workspaceOnly(),
      sessionId: sessionId(),
      effort: backendEffort(mode()),
      mode: devMode(),
      appDb: props.appDb,
      // Reuse the worktree once the session owns one; the backend creates it on
      // the first prompt (named after the work) when these are absent.
      workspaceId: focusedAgent().workspaceId,
      stateDb: focusedAgent().stateDb,
    }, {
      onEvent(event) {
        if (event.session_id && !focusedAgentSessionMatches(key, event.session_id)) {
          patchAgent(key, { sessionId: event.session_id })
          // First turn just created this session's worktree — surface it (and
          // its work-derived branch) in the sidebar without waiting for exit.
          void refreshWorktrees()
        }
        if (event.type === "metadata_updated") {
          if (event.display_name) updateAgentState(key, (next) => ({ ...next, title: event.display_name ?? next.title }))
          if (event.workspace_id || event.worktree_path || event.branch_name) void refreshWorktrees()
        }
        if (event.type === "permission_request" && store.focusedKey === key) setPermissionSelected(0)
        if (flags.stopping) {
          if (event.type === "result" || event.type === "error") {
            updateAgentState(key, (next) => markAgentStopped(next))
          }
          return
        }
        updateAgentState(key, (next) => applySessionEvent(next, event))
      },
      onStderr(text) {
        const lines = visibleStderr(text)
        if (!lines || store.focusedKey !== key) return
        setNotice({ text: truncateRight(lines.replace(/\s+/g, " "), 120), tone: "muted" })
      },
      onExit(code) {
        if (flags.forceTimer) {
          clearTimeout(flags.forceTimer)
          flags.forceTimer = undefined
        }
        runs.delete(key)
        if (flags.exitAfter) {
          props.exitApp()
          return
        }
        if (flags.stopping) {
          flags.stopping = false
          setNotice({ text: "stopped agent", tone: "red" })
          updateAgentState(key, (next) => markAgentStopped(next))
          void refreshWorktrees()
          return
        }
        updateAgentState(key, (next) => ({ ...next, running: false, status: code === 0 ? "idle" : `exited ${code ?? "unknown"}` }))
        void refreshWorktrees()
      },
    })
    runs.set(key, run)
  }

  function focusedAgentSessionMatches(key: string, sessionId: string) {
    const idx = agentIndex(key)
    return idx >= 0 && store.agents[idx].sessionId === sessionId
  }

  function updateDraft(value: string) {
    setDraft(value)
    if (replacingPrompt) return
    setPromptHistory((current) => current.index === undefined && !current.draft ? current : { ...current, index: undefined, draft: "" })
    const activeMention = findActiveMention(value)
    if (activeMention) {
      setMention(activeMention)
      openPalette("files")
      return
    }
    if (palette() === "files") {
      setMention(undefined)
      setPalette(undefined)
      setSelected(0)
    }
    if (value.startsWith("/")) openPalette("commands")
    if (!value.startsWith("/") && palette() === "commands") setPalette(undefined)
    void normalizeImagePathPaste(value)
  }

  function recordHistory(value: string) {
    setPromptHistory((current) => {
      const entries = recordPromptHistory(current.entries, value)
      if (entries !== current.entries) savePromptHistoryFile(promptHistoryPath, entries)
      return { entries, draft: "" }
    })
  }

  function replacePrompt(value: string) {
    replacingPrompt = true
    input.setText(value)
    input.cursorOffset = value.length
    setDraft(value)
    queueMicrotask(() => {
      replacingPrompt = false
    })
  }

  function insertPromptNewline() {
    const next = insertTextAtCursor(input.plainText, "\n", input.cursorOffset)
    input.setText(next.value)
    input.cursorOffset = next.cursorOffset
    updateDraft(next.value)
  }

  function navigatePromptHistory(direction: HistoryDirection) {
    if (!shouldNavigateHistory(input.plainText, input.cursorOffset, direction)) return false
    const result = stepPromptHistory(promptHistory(), input.plainText, direction)
    if (!result.moved) return false
    setPromptHistory(result.state)
    replacePrompt(result.value)
    return true
  }

  function openPalette(kind: PaletteKind) {
    setPalette(kind)
    setSelected(0)
  }

  function moveSelection(delta: number) {
    const count = paletteItems().length
    if (count === 0) return
    setSelected((index) => (index + delta + count) % count)
  }

  function acceptPalette(insertDirectory = false) {
    const item = paletteItems()[selected()]
    if (!item) return
    if (palette() === "files") {
      acceptFileChoice(item as FileChoice, insertDirectory)
      return
    }
    if (palette() === "models") {
      const choice = item as ModelChoice
      setProvider(choice.provider)
      setModel(choice.model)
      setMode(coerceEffortForModel(mode(), choice))
      closePalette()
      return
    }
    if (palette() === "connect") {
      connectProvider(item as ConnectChoice)
      return
    }
    if (palette() === "agents") {
      const choice = item as AgentChoice
      setAgent(choice.name)
      closePalette()
      return
    }
    if (palette() === "modes") {
      const choice = item as EffortChoice
      setMode(choice.value)
      closePalette()
      return
    }
    if (palette() === "permissions") {
      const choice = item as PermissionChoice
      setApproval(choice.approval)
      setWorkspaceOnly(choice.workspaceOnly)
      setNotice({ text: `permissions set to ${choice.label.toLowerCase()}`, tone: "muted" })
      closePalette()
      return
    }
    runCommand(item as Command)
  }

  function choosePalette(index: number) {
    setSelected(index)
    const item = paletteItems()[index]
    if (!item) return
    if (palette() === "files") {
      acceptFileChoice(item as FileChoice)
      return
    }
    if (palette() === "models") {
      const choice = item as ModelChoice
      setProvider(choice.provider)
      setModel(choice.model)
      setMode(coerceEffortForModel(mode(), choice))
      closePalette()
      return
    }
    if (palette() === "connect") {
      connectProvider(item as ConnectChoice)
      return
    }
    if (palette() === "agents") {
      const choice = item as AgentChoice
      setAgent(choice.name)
      closePalette()
      return
    }
    if (palette() === "modes") {
      const choice = item as EffortChoice
      setMode(choice.value)
      closePalette()
      return
    }
    if (palette() === "permissions") {
      const choice = item as PermissionChoice
      setApproval(choice.approval)
      setWorkspaceOnly(choice.workspaceOnly)
      setNotice({ text: `permissions set to ${choice.label.toLowerCase()}`, tone: "muted" })
      closePalette()
      return
    }
    runCommand(item as Command)
  }

  function connectProvider(choice: ConnectChoice) {
    const nextModel = defaultModel(choice.provider)
    const nextChoice = selectedModelChoice(choice.provider, nextModel) ?? modelChoices[0]
    setProvider(choice.provider)
    setModel(nextModel)
    if (nextChoice) setMode(coerceEffortForModel(mode(), nextChoice))
    closePalette()
    if (choice.provider !== "copilot") {
      setNotice({ text: `${choice.label.toLowerCase()} selected`, tone: "muted" })
      return
    }

    setNotice({ text: "copilot login starting...", tone: "cyan" })
    setCopilotDeviceNotice(undefined)
    void startCopilotLogin(props, handleCopilotAuthStatus).catch((error) => {
      setCopilotDeviceNotice(undefined)
      setNotice({ text: `copilot login failed: ${String(error?.message ?? error)}`, tone: "red" })
    })
  }

  function handleCopilotAuthStatus(event: AuthStatusEvent) {
    if (event.provider && event.provider !== "copilot") return
    if (event.status === "device_code") {
      if (event.verification_uri) {
        try {
          Bun.spawn(["open", event.verification_uri], { stdout: "ignore", stderr: "ignore" })
        } catch {
          // Keep the code visible when macOS cannot open the browser.
        }
      }
      const deviceNotice = {
        text: `copilot: enter ${event.user_code ?? "code"} at ${event.verification_uri ?? "github.com/login/device"}`,
        tone: "cyan",
      } as const
      setCopilotDeviceNotice(deviceNotice)
      setNotice(deviceNotice)
      return
    }
    if (event.status === "waiting") {
      setNotice(copilotDeviceNotice() ?? { text: "copilot: waiting for browser approval", tone: "cyan" })
      return
    }
    if (event.status === "connected") {
      setCopilotDeviceNotice(undefined)
      setNotice({ text: "copilot connected", tone: "cyan" })
      void refreshCopilotModels()
      return
    }
    if (event.status === "expired") {
      setCopilotDeviceNotice(undefined)
      setNotice({ text: "copilot login expired", tone: "red" })
      return
    }
    if (event.status === "failed") {
      setCopilotDeviceNotice(undefined)
      setNotice({ text: `copilot login failed: ${event.message ?? "unknown error"}`, tone: "red" })
    }
  }

  function closePalette() {
    setPalette(undefined)
    setMention(undefined)
    setSelected(0)
    input.setText("")
    setDraft("")
    queueMicrotask(() => input.focus())
  }

  async function refreshCopilotModels() {
    try {
      const models = await listProviderModels(props, "copilot")
      if (models.length === 0) return
      const next = [
        ...modelChoices.filter((choice) => choice.provider !== "copilot"),
        ...models.map(copilotModelChoice),
      ]
      modelChoices = next
      setModelCatalogVersion((version) => version + 1)
    } catch {
      // Keep the baked-in Copilot fallback choices when auth is absent or stale.
    }
  }

  function acceptFileChoice(choice: FileChoice, insertDirectory = false) {
    const active = mention()
    if (!active) return
    const next = replaceMention(input.plainText, active, choice, insertDirectory)
    input.setText(next)
    input.cursorOffset = next.length
    setDraft(next)
    if (choice.kind === "dir" && !insertDirectory) {
      const nextMention = findActiveMention(next)
      setMention(nextMention)
      setSelected(0)
      setPalette("files")
      return
    }
    setMention(undefined)
    setPalette(undefined)
    setSelected(0)
    queueMicrotask(() => {
      input.focus()
      input.cursorOffset = next.length
    })
  }

  async function pasteFromClipboard() {
    const payload = await readClipboard()
    if (!payload) return
    if (payload.type === "image") {
      const rel = writePastedImage(Buffer.from(payload.base64, "base64"))
      insertImagePlaceholder(rel)
      return
    }

    const normalized = attachImagePathTokens(payload.text)
    insertPromptText(normalized)
  }

  function insertPromptToken(token: string) {
    const next = appendPromptToken(input.plainText, token)
    input.setText(next)
    updateDraft(next)
  }

  function insertImagePlaceholder(rel: string) {
    const nextIndex = promptImages().length + 1
    const label = `[Image #${nextIndex}]`
    setPromptImages((current) => [...current, { label, path: rel }])
    const next = appendPromptToken(input.plainText, label)
    input.setText(next)
    input.cursorOffset = next.length
    setDraft(next)
  }

  function insertPromptText(text: string) {
    const next = `${input.plainText}${text}`
    input.setText(next)
    updateDraft(next)
  }

  function appendAssistantMessage(text: string) {
    const key = store.focusedKey
    updateAgentState(key, (next) => ({
      ...next,
      transcript: [...next.transcript, { id: `pr-${Date.now()}`, kind: "assistant", text }],
    }))
  }

  async function normalizeImagePathPaste(value: string) {
    const normalized = attachImagePathTokens(value)
    if (normalized === value) return
    input.setText(normalized)
    input.cursorOffset = normalized.length
    setDraft(normalized)
  }

  function attachImagePathTokens(value: string) {
    return value.replace(/(^|\s)(?!@|\[Image)("[^"]+"|'[^']+'|\S+\.(?:png|jpe?g|gif|webp|bmp|tiff?))/gi, (match, prefix: string, token: string) => {
      const clean = stripToken(token)
      if (!isImagePath(clean)) return match
      const rel = materializeImagePath(clean)
      if (!rel) return match
      const nextIndex = promptImages().length + 1
      const label = `[Image #${nextIndex}]`
      setPromptImages((current) => [...current, { label, path: rel }])
      return `${prefix}${label}`
    })
  }

  function materializeImagePath(rawPath: string) {
    const absolute = path.isAbsolute(rawPath) ? rawPath : path.resolve(props.workspace, rawPath)
    if (!existsSync(absolute)) return undefined
    const workspaceRoot = path.resolve(props.workspace)
    if (absolute.startsWith(`${workspaceRoot}${path.sep}`)) {
      return toWorkspacePath(path.relative(workspaceRoot, absolute))
    }
    const extension = path.extname(absolute) || ".png"
    const rel = attachmentPath("dropped", extension)
    mkdirSync(path.dirname(path.join(props.workspace, rel)), { recursive: true })
    copyFileSync(absolute, path.join(props.workspace, rel))
    return rel
  }

  function writePastedImage(bytes: Buffer) {
    const rel = attachmentPath("pasted", ".png")
    const absolute = path.join(props.workspace, rel)
    mkdirSync(path.dirname(absolute), { recursive: true })
    writeFileSync(absolute, bytes)
    return rel
  }

  function attachmentPath(kind: "pasted" | "dropped", extension: string) {
    const next = pasteCount() + 1
    setPasteCount(next)
    const filename = kind === "pasted" ? pastedImageName(next, extension) : `dropped-image-${next}${extension}`
    return toWorkspacePath(path.join(".inductor", "attachments", filename))
  }

  function runCommand(command: Command) {
    if (command.action === "exit") {
      props.exitApp()
      return
    }
    recordHistory(command.name)
    if (command.action === "new" || command.action === "clear") {
      startNewSession()
      closePalette()
      return
    }
    if (command.action === "model") {
      openPalette("models")
      return
    }
    if (command.action === "connect") {
      openPalette("connect")
      return
    }
    if (command.action === "agents") {
      openPalette("agents")
      return
    }
    if (command.action === "mode") {
      openPalette("modes")
      return
    }
    if (command.action === "permissions") {
      openPalette("permissions")
      return
    }
    if (command.action === "pr") {
      closePalette()
      startPullRequestFlow()
      return
    }
    if (command.action === "review") {
      input.setText("review current changes")
      setDraft("review current changes")
      setPalette(undefined)
      submit()
      return
    }
    closePalette()
  }

  function focusedWorktree() {
    const agent = focusedAgent()
    return worktrees().find((w) =>
      (agent.sessionId && agent.sessionId === w.session_id) ||
      (agent.workspaceId && agent.workspaceId === w.workspace_id)
    )
  }

  function startPullRequestFlow() {
    const agent = focusedAgent()
    const worktree = focusedWorktree()
    if (!worktree) {
      setNotice({ text: "No worktree yet — send a prompt first", tone: "cyan" })
      return
    }
    if (!worktree.exists) {
      setNotice({ text: "Cannot create PR for missing worktree", tone: "red" })
      return
    }
    if (runs.has(agent.key) || agent.state.running) {
      setNotice({ text: "Stop this agent before creating a PR", tone: "cyan" })
      return
    }
    setPrFlow({ step: "base", base: "main", worktree })
    replacePrompt("main")
    setNotice({ text: "PR base branch (default main) · edit or press enter", tone: "cyan" })
  }

  async function submitPrFlow(value: string) {
    const flow = prFlow()
    if (!flow) return
    if (flow.step === "base") {
      const base = value.trim() || "main"
      setPrFlow({ ...flow, step: "message", base })
      replacePrompt("")
      setNotice({ text: `PR commit message · enter to commit, push, and create PR against ${base}`, tone: "cyan" })
      return
    }
    const message = value.trim()
    if (!message) {
      setNotice({ text: "Commit message is required for /pr", tone: "red" })
      return
    }
    input.setText("")
    setDraft("")
    setPrFlow(undefined)
    recordHistory(`/pr ${flow.base} ${message}`)
    await createPullRequestForWorktree(flow.worktree, flow.base, message)
  }

  async function createPullRequestForWorktree(worktree: Worktree, base: string, message: string) {
    setWorktreeBusy(worktree.workspace_id)
    setNotice({ text: `creating PR for ${worktree.branch_name} against ${base}...`, tone: "cyan" })
    try {
      await execFileAsync("git", ["-C", worktree.worktree_path, "add", "-A"], { maxBuffer: 1024 * 1024 * 4 })
      const { stdout: status } = await execFileAsync("git", ["-C", worktree.worktree_path, "status", "--porcelain"], { maxBuffer: 1024 * 1024 })
      if (status.trim()) {
        await execFileAsync("git", ["-C", worktree.worktree_path, "commit", "-m", message], { maxBuffer: 1024 * 1024 * 4 })
      }
      await execFileAsync("git", ["-C", worktree.worktree_path, "push", "origin", worktree.branch_name], { maxBuffer: 1024 * 1024 * 4 })
      const { stdout } = await execFileAsync("gh", ["pr", "create", "--repo", worktree.source_repo, "--head", worktree.branch_name, "--base", base, "--title", message, "--body", "Created by Inductor.", "--json", "url", "--jq", ".url"], { cwd: worktree.worktree_path, maxBuffer: 1024 * 1024 })
      const url = stdout.trim()
      setNotice({ text: url ? `PR created: ${url}` : "PR created", tone: "cyan" })
      if (url) appendAssistantMessage(`✅ Pull request created against ${base}:\n${url}`)
      await refreshWorktrees()
    } catch (error) {
      setNotice({ text: pullRequestErrorMessage(error), tone: "red" })
    } finally {
      setWorktreeBusy(undefined)
    }
  }

  function agentForWorktree(worktree: Worktree) {
    return store.agents.find((a) => (a.sessionId && a.sessionId === worktree.session_id) || (a.workspaceId && a.workspaceId === worktree.workspace_id))
  }

  // Open a new session. The worktree itself is created lazily on the first
  // prompt so it can be named after the work (the backend derives the branch,
  // e.g. `inductor/terminal-bug-fix`). Until then the slot shows as a draft.
  function startNewSession() {
    const base = focusedAgent()
    // Reuse a pristine focused slot rather than piling up empty agents.
    if (base.state.transcript.length === 0 && !base.state.running && !base.sessionId) {
      setExpanded(new Set<string>())
      setNotice(undefined)
      queueMicrotask(() => input?.focus())
      return
    }
    const slot = makeAgentSlot({ provider: base.provider, model: base.model, effort: base.effort, devMode: base.devMode, approval: base.approval, workspaceOnly: base.workspaceOnly, role: base.role })
    setStore("agents", (agents) => [...agents, slot])
    setStore("focusedKey", slot.key)
    setExpanded(new Set<string>())
    setNotice(undefined)
    queueMicrotask(() => input?.focus())
  }

  async function archiveWorktreeAction(worktree: Worktree) {
    const open = agentForWorktree(worktree)
    if (open?.state.running) {
      setNotice({ text: "Stop this worktree's agent before archiving", tone: "cyan" })
      return
    }
    setWorktreeBusy(worktree.workspace_id)
    setNotice({ text: `archiving ${worktree.branch_name}...`, tone: "cyan" })
    try {
      await archiveWorktree(props, worktree.workspace_id)
      // Drop the live slot too, otherwise the now-worktree-less agent would
      // resurface as a draft row — archived sessions should leave the sidebar.
      if (open) closeAgentSlot(open.key)
      setNotice({ text: "worktree archived (chats kept)", tone: "muted" })
      await refreshWorktrees()
    } catch (error) {
      setNotice({ text: error instanceof Error ? error.message : "archive failed", tone: "red" })
    } finally {
      setWorktreeBusy(undefined)
    }
  }

  // Remove an agent slot from the sidebar, tearing down any lingering run. If it
  // was the last slot (or the focused one), fall back to a fresh session so the
  // composer always has a slot to write into.
  function closeAgentSlot(key: string) {
    runs.get(key)?.kill()
    runs.delete(key)
    runFlags.delete(key)
    const remaining = store.agents.filter((a) => a.key !== key)
    if (remaining.length === 0) {
      const base = store.agents.find((a) => a.key === key) ?? store.agents[0]
      const fresh = makeAgentSlot({ provider: base.provider, model: base.model, effort: base.effort, devMode: base.devMode, approval: base.approval, workspaceOnly: base.workspaceOnly, role: base.role })
      setStore({ agents: [fresh], focusedKey: fresh.key })
      setExpanded(new Set<string>())
      queueMicrotask(() => input?.focus())
      return
    }
    setStore("agents", (agents) => agents.filter((a) => a.key !== key))
    if (store.focusedKey === key) focusAgent(remaining[0].key)
  }

  async function refreshWorktrees() {
    try {
      const next = await listWorktrees(props)
      // Archived worktrees keep their chats but no longer have a working
      // directory — hide them from the sidebar so it only lists live worktrees.
      setWorktrees(next.filter((w) => w.status !== "archived"))
      setSessionListStatus("")
    } catch (error) {
      setSessionListStatus(error instanceof Error ? error.message : "Could not load worktrees")
    }
  }

  function focusAgent(key: string) {
    setStore("focusedKey", key)
    setExpanded(new Set<string>())
    setNotice(undefined)
    queueMicrotask(() => input?.focus())
  }

  async function loadWorktree(worktree: Worktree) {
    if (!worktree.session_id) {
      setNotice({ text: "This worktree has no session yet", tone: "muted" })
      return
    }
    // Already open as a live agent — just focus it (keeps it running).
    const existing = store.agents.find((a) => a.sessionId === worktree.session_id)
    if (existing) {
      focusAgent(existing.key)
      return
    }
    try {
      const detail = await showWorkspaceSession(props, worktree.session_id, worktree.state_db ?? undefined)
      const slot = makeAgentSlot({
        sessionId: worktree.session_id,
        workspaceId: worktree.workspace_id,
        branch: worktree.branch_name,
        provider: sessionProvider(detail.session.provider_id) ?? props.provider,
        model: detail.session.model || (props.model ?? defaultModel(props.provider)),
        devMode: "worktree",
        stateDb: worktree.state_db ?? undefined,
        state: loadStoredSession(detail),
      })
      setStore("agents", (agents) => [...agents, slot])
      setStore("focusedKey", slot.key)
      setExpanded(new Set<string>())
      setNotice({ text: worktree.exists ? "session loaded" : "session loaded (worktree archived — read-only)", tone: "muted" })
      queueMicrotask(() => input?.focus())
    } catch (error) {
      setNotice({ text: error instanceof Error ? error.message : "Could not load session", tone: "red" })
    }
  }

  function decide(decision: PermissionDecision) {
    const key = store.focusedKey
    const request = fstate().pendingPermission
    const run = runs.get(key)
    if (!request || !run) return
    run.respond(request.requestId, decision)
    setPermissionSelected(0)
    updateAgentState(key, (next) => applyPermissionDecision(next, decision))
  }

  function handlePermissionKey(event: KeyEvent) {
    const key = permissionKey(event)
    if (key === "arrowup" || key === "up" || key === "k") {
      setPermissionSelected((index) => (index + permissionActions.length - 1) % permissionActions.length)
      return true
    }
    if (key === "arrowdown" || key === "down" || key === "j") {
      setPermissionSelected((index) => (index + 1) % permissionActions.length)
      return true
    }
    if (key === "enter" || key === "return" || key === "\r" || key === "\n") {
      decide(permissionActions[permissionSelected()] ?? "allow")
      return true
    }
    if (key === "1" || key === "y") {
      decide("allow")
      return true
    }
    if (key === "2" || key === "a") {
      decide("allow_always")
      return true
    }
    if (key === "3" || key === "n") {
      decide("deny")
      return true
    }
    return false
  }

  function toggleExpanded(id: string) {
    setExpanded((current) => {
      const next = new Set(current)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  function handleEsc() {
    if (stopArmed() === "interrupt") {
      stopCurrentRun(false)
      return
    }
    armStop("interrupt", "Press Esc again to stop the agent")
  }

  function handleCtrlC() {
    const focusedRunning = fstate().running || runs.has(store.focusedKey)
    if (stopArmed() === "exit") {
      if (focusedRunning) {
        stopCurrentRun(true)
      } else {
        props.exitApp()
      }
      return
    }
    armStop("exit", focusedRunning ? "Press Ctrl+C again to stop the agent and quit" : "Press Ctrl+C again to quit Inductor")
  }

  function armStop(intent: StopIntent, text: string) {
    setStopArmed(intent)
    setNotice({ text, tone: "cyan" })
    clearStopArmTimer()
    stopArmTimer = setTimeout(() => {
      setStopArmed(undefined)
      setNotice(undefined)
    }, 5000)
  }

  function disarmStopWarning() {
    if (!stopArmed()) return
    setStopArmed(undefined)
    clearStopArmTimer()
    setNotice(undefined)
  }

  function stopCurrentRun(quitAfterStop: boolean) {
    const key = store.focusedKey
    const run = runs.get(key)
    if (!run) {
      if (quitAfterStop) props.exitApp()
      setNotice({ text: "No running agent to stop", tone: "muted" })
      setStopArmed(undefined)
      return
    }

    const flags = runFlagsFor(key)
    flags.stopping = true
    flags.exitAfter = quitAfterStop
    setStopArmed(undefined)
    clearStopArmTimer()
    setNotice({ text: quitAfterStop ? "Stopping agent, then quitting Inductor..." : "Stopping agent...", tone: "cyan" })
    updateAgentState(key, (next) => markAgentStopped(next))
    run.interrupt()
    if (flags.forceTimer) clearTimeout(flags.forceTimer)
    flags.forceTimer = setTimeout(() => {
      if (!flags.stopping) return
      run.kill()
      if (flags.exitAfter) props.exitApp()
    }, 5000)
  }

  function clearStopArmTimer() {
    if (!stopArmTimer) return
    clearTimeout(stopArmTimer)
    stopArmTimer = undefined
  }

  function openModifiedFile(file: ModifiedFile) {
    openExternalDiffViewer(props.workspace, file)
  }

  return (
    <box width="100%" height="100%" backgroundColor={theme.bg} paddingTop={1} paddingLeft={1} paddingRight={1} paddingBottom={1}>
      <box width="100%" height="100%" backgroundColor={theme.bg} flexDirection="column" border borderStyle="rounded" borderColor={theme.border}>
        <TopRail
          mode={mode()}
          agent={agent()}
          provider={provider()}
          model={model()}
          title={fstate().title}
          workspace={props.workspace}
          running={fstate().running}
          elapsed={formatElapsed(now() - startedAt)}
          branch={activeBranch()}
          openPalette={openPalette}
        />
        <box flexGrow={1} minHeight={0} overflow="hidden" flexDirection="row" gap={1} paddingLeft={1} paddingRight={1} paddingTop={1} paddingBottom={1}>
          <SessionSidebar
            agents={store.agents}
            worktrees={worktrees()}
            focusedKey={store.focusedKey}
            currentSessionId={sessionId()}
            status={sessionListStatus()}
            devMode={devMode()}
            busyId={worktreeBusy()}
            focusAgent={focusAgent}
            loadWorktree={loadWorktree}
            archiveWorktree={archiveWorktreeAction}
            terminalSnapshot={terminalSnapshot}
            terminalError={terminalError}
            terminalWrite={terminalWrite}
            terminalCwd={focusedWorktreePath()}
          />
          <box
            flexGrow={1}
            minWidth={0}
                flexDirection="column"
            backgroundColor={theme.panel}
                border
            borderStyle="rounded"
            borderColor={theme.border}
          >
            <Show when={hasTranscript()} fallback={<StartScreen height={dimensions().height} />}>
              <scrollbox
                flexGrow={1}
                minHeight={0}
                        stickyScroll={true}
                stickyStart="bottom"
                scrollAcceleration={scrollAcceleration}
                viewportCulling={true}
                viewportOptions={{ overflow: "hidden" }}
                contentOptions={{ overflow: "hidden" }}
                verticalScrollbarOptions={{ visible: false }}
              >
                <Timeline
                  items={fstate().transcript}
                  pendingPermission={fstate().pendingPermission}
                  running={fstate().running}
                  runningStatus={fstate().status}
                  activityGlyph={runningGlyph(now())}
                  permissionSelected={permissionSelected()}
                  selectPermission={setPermissionSelected}
                  expanded={expanded()}
                  toggleExpanded={toggleExpanded}
                  decide={decide}
                />
              </scrollbox>
            </Show>
          </box>
          <TelemetrySidebar
            state={fstate()}
            provider={provider()}
            model={model()}
            workspace={props.workspace}
            worktreePath={focusedWorktreePath()}
            contextPercent={contextPercent()}
            mode={mode()}
            branch={activeBranch()}
            openModifiedFile={openModifiedFile}
          />
        </box>
        <Composer
          state={fstate()}
          provider={provider()}
          model={model()}
          mode={mode()}
          agent={agent()}
          inputRef={(ref) => (input = ref)}
          draft={draft}
          setDraft={updateDraft}
          submit={submit}
          palette={palette}
          paletteItems={paletteItems}
          selected={selected}
          moveSelection={moveSelection}
          acceptPalette={acceptPalette}
          choosePalette={choosePalette}
          openPalette={openPalette}
          insertPromptNewline={insertPromptNewline}
          navigatePromptHistory={navigatePromptHistory}
          notice={composerNotice()}
          pasteFromClipboard={pasteFromClipboard}
        />
      </box>
    </box>
  )
}

function TopRail(props: {
  mode: EffortValue
  agent: string
  provider: string
  model: string
  title: string
  workspace: string
  running: boolean
  elapsed: string
  branch: string
  openPalette: (kind: PaletteKind) => void
}) {
  return (
    <box
      width="100%"
      height={4}
      flexShrink={0}
      flexDirection="row"
      backgroundColor={theme.surface}
      border={["bottom"]}
      borderColor={theme.border}
    >
      <TopBrand />
      <TopMetric width={22} label="effort" value={props.mode} color={theme.cyan} onClick={() => props.openPalette("modes")} />
      <TopMetric width={34} label="agent" value={truncateRight(modelDisplay(props.provider, props.model), 20)} color={theme.blue} onClick={() => props.openPalette("models")} />
      <TopMetric width={32} label="session" value={truncateRight(props.title, 18)} color={theme.cyan} />
      <TopMetric width={28} label="branch" value={truncateRight(props.branch, 18)} color={theme.cyan} />
      <box flexGrow={1} height="100%" />
      <box width={18} height="100%" flexDirection="row" alignItems="center" justifyContent="center">
        <text fg={props.running ? theme.green : theme.text} attributes={TextAttributes.BOLD}>◴ {clockElapsed(props.elapsed)}</text>
      </box>
    </box>
  )
}

function TopBrand() {
  return (
    <box
      width={26}
      height="100%"
      flexDirection="row"
      alignItems="center"
      paddingLeft={3}
      paddingRight={3}
      border={["right"]}
      borderColor={theme.border}
    >
      <text fg={theme.cyan} attributes={TextAttributes.BOLD}>INDUCTOR</text>
    </box>
  )
}

function TopMetric(props: { width: number; label: string; value: string; color: string; onClick?: () => void }) {
  return (
    <box
      width={props.width}
      height="100%"
      flexDirection="row"
      alignItems="center"
      paddingLeft={3}
      paddingRight={3}
      gap={2}
      border={["right"]}
      borderColor={theme.border}
      onMouseUp={props.onClick}
    >
      <text fg={theme.dim} wrapMode="none">{props.label}</text>
      <text fg={props.color} attributes={TextAttributes.BOLD} wrapMode="none">{props.value}</text>
    </box>
  )
}

function SessionSidebar(props: {
  agents: AgentSlot[]
  worktrees: Worktree[]
  focusedKey: string
  currentSessionId?: string
  status: string
  devMode: DevMode
  busyId?: string
  focusAgent: (key: string) => void
  loadWorktree: (worktree: Worktree) => void
  archiveWorktree: (worktree: Worktree) => void
  terminalSnapshot: () => TerminalSnapshot | undefined
  terminalError: () => string | undefined
  terminalWrite: (data: string) => void
  terminalCwd: string
}) {
  const agentForWorktree = (worktree: Worktree) =>
    props.agents.find((a) => (a.sessionId && a.sessionId === worktree.session_id) || (a.workspaceId && a.workspaceId === worktree.workspace_id))
  const draftAgents = () => props.agents.filter((agent) =>
    !props.worktrees.some((worktree) =>
      (agent.sessionId && agent.sessionId === worktree.session_id) ||
      (agent.workspaceId && agent.workspaceId === worktree.workspace_id)
    )
  )
  const sessionCount = () => props.worktrees.length + draftAgents().length
  return (
    <box
      width={SESSION_SIDEBAR_WIDTH}
      height="100%"
      flexShrink={0}
      backgroundColor={theme.panelSoft}
      border
      borderStyle="rounded"
      borderColor={theme.border}
      paddingTop={1}
      paddingLeft={1}
      paddingRight={1}
      paddingBottom={1}
      flexDirection="column"
    >
      <box flexGrow={1} minHeight={0} flexDirection="column">
        <box flexDirection="row" gap={1} paddingLeft={1} paddingRight={1} marginBottom={1}>
          <text fg={theme.cyan}>WORKTREES</text>
          <box flexGrow={1} />
          <text fg={theme.dim}>{sessionCount()}</text>
        </box>
        <Show when={!props.status} fallback={<text fg={theme.red}>{truncateRight(props.status, SESSION_SIDEBAR_TEXT_WIDTH)}</text>}>
          <scrollbox flexGrow={1} minHeight={0} scrollAcceleration={scrollAcceleration} verticalScrollbarOptions={{ visible: false }}>
            <box flexDirection="column" gap={1}>
              <For each={draftAgents()}>
                {(slot) => (
                  <DraftSessionRow
                    slot={slot}
                    focused={props.focusedKey === slot.key}
                    focus={() => props.focusAgent(slot.key)}
                  />
                )}
              </For>
              <For each={props.worktrees}>
                {(worktree) => {
                  const agent = () => agentForWorktree(worktree)
                  return (
                    <WorktreeRow
                      worktree={worktree}
                      agent={agent()}
                      active={props.currentSessionId === worktree.session_id || agent()?.key === props.focusedKey}
                      busy={props.busyId === worktree.workspace_id}
                      load={() => {
                        const open = agent()
                        if (open) props.focusAgent(open.key)
                        else props.loadWorktree(worktree)
                      }}
                      archive={() => props.archiveWorktree(worktree)}
                    />
                  )
                }}
              </For>
            </box>
          </scrollbox>
        </Show>
      </box>
      <box width="100%" height={1} border={["top"]} borderColor={theme.borderSoft} marginTop={1} marginBottom={1} />
      <TerminalPanel
        snapshot={props.terminalSnapshot}
        error={props.terminalError}
        write={props.terminalWrite}
        cwd={props.terminalCwd}
      />
    </box>
  )
}

function TerminalPanel(props: {
  snapshot: () => TerminalSnapshot | undefined
  error: () => string | undefined
  write: (data: string) => void
  cwd: string
}) {
  let surface!: BoxRenderable
  // The block cursor blinks like a native terminal whenever the shell is live
  // so the user can always see where input will land, regardless of which pane
  // currently holds keyboard focus.
  const [blinkOn, setBlinkOn] = createSignal(true)
  const blinkTimer = setInterval(() => setBlinkOn((on) => !on), 530)
  onCleanup(() => clearInterval(blinkTimer))
  const cursorVisible = () => blinkOn()
  // vt100 screen contents render as a fixed grid; strip trailing blank rows so
  // the prompt hugs the bottom instead of padding out the panel.
  const snapshotLines = () => {
    const snapshot = props.snapshot()
    if (!snapshot) return { rows: [] as string[], cursorRow: -1, cursorCol: 0 }
    const grid = snapshot.contents.split("\n")
    let end = grid.length
    while (end > 0 && grid[end - 1].trim() === "") end -= 1
    // Keep enough rows to still show where the cursor sits, even on a blank line.
    const visible = Math.max(end, snapshot.cursor_row + 1)
    return { rows: grid.slice(0, visible), cursorRow: snapshot.cursor_row, cursorCol: snapshot.cursor_col }
  }
  const running = () => !props.error() && props.snapshot()?.is_running !== false
  const status = () => (props.error() ? "unavailable" : running() ? "live" : "exited")
  // Pass typed keys straight through to the PTY as raw bytes so the shell
  // echoes them itself — the prompt, cursor, and line wrapping all come from
  // the real terminal, exactly like a native shell.
  const forwardKey = (event: KeyEvent) => {
    if (!running()) return
    const data = event.sequence
    if (!data) return
    props.write(data)
    event.preventDefault()
    event.stopPropagation()
  }
  return (
    <box
      flexGrow={1}
      minHeight={0}
      flexDirection="column"
      focusable={true}
      onMouseUp={() => surface.focus()}
      onKeyDown={forwardKey}
      ref={(ref: BoxRenderable) => {
        surface = ref
        // Reset the blink to "on" when focused so the cursor is solid the
        // instant the user clicks in, then resumes blinking.
        ref.on("focused", () => setBlinkOn(true))
      }}
    >
      <box flexDirection="row" gap={1} paddingLeft={1} paddingRight={1} marginBottom={1}>
        <text fg={theme.cyan}>TERMINAL</text>
        <box flexGrow={1} />
        <text fg={props.error() ? theme.red : running() ? theme.green : theme.dim}>{status()}</text>
      </box>
      <scrollbox
        flexGrow={1}
        minHeight={0}
        stickyScroll={true}
        stickyStart="bottom"
        scrollAcceleration={scrollAcceleration}
        verticalScrollbarOptions={{ visible: false }}
      >
        <box flexDirection="column" paddingLeft={1} paddingRight={1}>
          <Show when={snapshotLines().rows.length > 0} fallback={<text fg={props.error() ? theme.red : theme.dim}>{props.error() ?? "starting shell…"}</text>}>
            <For each={snapshotLines().rows}>
              {(line, index) => (
                <TerminalLine
                  text={line}
                  cursorCol={running() && cursorVisible() && index() === snapshotLines().cursorRow ? snapshotLines().cursorCol : -1}
                />
              )}
            </For>
          </Show>
        </box>
      </scrollbox>
    </box>
  )
}

/**
 * One row of the terminal grid. When `cursorCol >= 0` the cell at that column
 * is drawn as an inverse block so the cursor sits inline right after the
 * prompt, matching a native shell.
 */
function TerminalLine(props: { text: string; cursorCol: number }) {
  if (props.cursorCol < 0) {
    return <text fg={theme.muted} wrapMode="char" selectable={true}>{props.text.length ? props.text : " "}</text>
  }
  const padded = props.text.length < props.cursorCol ? props.text.padEnd(props.cursorCol, " ") : props.text
  const before = padded.slice(0, props.cursorCol)
  const at = padded.slice(props.cursorCol, props.cursorCol + 1) || " "
  const after = padded.slice(props.cursorCol + 1)
  return (
    <box flexDirection="row">
      <text fg={theme.muted} wrapMode="none" selectable={true}>{before}</text>
      <text fg="#0a1014" bg={theme.cyan} attributes={TextAttributes.BOLD}>{at}</text>
      <text fg={theme.muted} selectable={true}>{after}</text>
    </box>
  )
}

function DraftSessionRow(props: {
  slot: AgentSlot
  focused: boolean
  focus: () => void
}) {
  const s = () => props.slot.state
  const statusColor = () => s().pendingPermission ? theme.orange : s().running ? theme.green : theme.dim
  return (
    <box
      width="100%"
      flexDirection="column"
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={props.focused ? theme.paletteSelected : theme.panelSoft}
      border={["left"]}
      borderColor={props.focused ? theme.cyan : s().running ? theme.green : theme.borderSoft}
    >
      <box flexDirection="row" gap={1} onMouseUp={props.focus}>
        <text fg={s().running ? theme.green : theme.dim} selectable={false}>{s().running ? "●" : s().pendingPermission ? "?" : "○"}</text>
        <text fg={props.focused ? theme.text : theme.muted} attributes={props.focused ? TextAttributes.BOLD : undefined} wrapMode="none">
          {truncateRight(s().title || "New session", SESSION_SIDEBAR_TEXT_WIDTH - 6)}
        </text>
      </box>
      <box flexDirection="row" gap={1} onMouseUp={props.focus}>
        <text fg={theme.dim}>{providerLabel(props.slot.provider)} {truncateRight(shortModel(props.slot.model), 8)}</text>
        <box flexGrow={1} />
        <text fg={statusColor()}>{s().running ? "running" : s().pendingPermission ? "needs approval" : (s().status || "idle")}</text>
      </box>
    </box>
  )
}

function WorktreeRow(props: {
  worktree: Worktree
  agent?: AgentSlot
  active: boolean
  busy: boolean
  load: () => void
  archive: () => void
}) {
  const wt = props.worktree
  const title = wt.display_name || wt.branch_name.replace(/^inductor\//, "") || "session"
  const live = () => props.agent?.state
  const isRunning = () => Boolean(live()?.running)
  const needsApproval = () => Boolean(live()?.pendingPermission)
  const isMerged = () => wt.status === "merged"
  const rowStatus = () => {
    if (props.busy) return "working..."
    if (isRunning()) return "running"
    if (needsApproval()) return "needs approval"
    return wt.status
  }
  const statusColor = () => {
    if (needsApproval()) return theme.orange
    if (isRunning()) return theme.green
    if (wt.status === "active" || wt.status === "pr_open") return theme.cyan
    if (isMerged()) return theme.purple
    return theme.dim
  }
  return (
    <box
      width="100%"
      flexDirection="column"
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={isMerged() ? "#24143a" : props.active ? theme.paletteSelected : theme.panelSoft}
      border={["left"]}
      borderColor={isMerged() ? theme.purple : props.active ? theme.cyan : isRunning() ? theme.green : theme.borderSoft}
    >
      <box flexDirection="row" gap={1} onMouseUp={props.load}>
        <text fg={isRunning() ? theme.green : needsApproval() ? theme.orange : theme.dim} selectable={false}>{isRunning() ? "●" : needsApproval() ? "?" : "○"}</text>
        <text fg={props.active ? theme.text : theme.muted} attributes={props.active ? TextAttributes.BOLD : undefined} wrapMode="none">
          {truncateRight(title, SESSION_SIDEBAR_TEXT_WIDTH - 6)}
        </text>
      </box>
      <box flexDirection="row" gap={1}>
        <text fg={statusColor()}>{rowStatus()}</text>
        <box flexGrow={1} />
        <Show when={wt.status !== "archived" && wt.exists && !isRunning()}>
          <text fg={theme.red} selectable={false} onMouseUp={props.archive}>archive</text>
        </Show>
      </box>
    </box>
  )
}

function pullRequestErrorMessage(error: unknown) {
  const anyError = error as { message?: string; stderr?: string; stdout?: string; code?: string | number }
  const detail = String(anyError?.stderr || anyError?.stdout || anyError?.message || error).trim().replace(/\s+/g, " ")
  if (detail.includes("executable file not found") || detail.includes("ENOENT")) return "PR failed: install/login to GitHub CLI (`gh`)"
  return truncateRight(`PR failed: ${detail || anyError?.code || "unknown error"}`, 140)
}

function defaultComposerNotice(status: string, running: boolean, pendingPermission?: AppState["pendingPermission"]): ComposerNotice {
  if (pendingPermission) return { text: "approval required · ↑/↓ choose · enter confirm", tone: "cyan" }
  if (!status || status === "idle") return { text: "ready", tone: "muted" }
  if (status === "stopped") return { text: "stopped agent", tone: "red" }
  if (running) return { text: status, tone: "muted" }
  return { text: status, tone: "muted" }
}

function noticeColor(notice: ComposerNotice) {
  if (notice.tone === "cyan") return theme.cyan
  if (notice.tone === "red") return theme.red
  return theme.muted
}

function isEscape(event: KeyEvent) {
  const name = event.name.toLowerCase()
  return name === "escape" || name === "esc" || event.sequence === "\x1b"
}

function isCtrlC(event: KeyEvent) {
  return (event.ctrl && event.name.toLowerCase() === "c") || event.sequence === "\x03"
}

// Ctrl+N opens a new worktree/session — mirrors the /new command.
function isNewSessionShortcut(event: KeyEvent) {
  return Boolean(event.ctrl) && event.name?.toLowerCase() === "n"
}

function permissionKey(event: KeyEvent) {
  return (event.name || event.sequence || "").toLowerCase()
}

function StartScreen(_props: { height: number }) {
  return (
    <box flexGrow={1} height="100%" flexDirection="column" alignItems="center" justifyContent="center">
      <box flexDirection="column" alignItems="center">
        <ascii_font
          text="inductor"
          font="block"
          color={["#6f7377", "#a7aaad", "#f2f3f4"]}
          backgroundColor={theme.panel}
          selectable={false}
        />
        <box marginTop={2}>
          <text fg={theme.muted} selectable={false}>- ask, edit, review, or run commands</text>
        </box>
        <box marginTop={1}>
          <text fg={theme.dim} selectable={false}>Ctrl+N or /new starts a new worktree</text>
        </box>
      </box>
    </box>
  )
}

function Timeline(props: {
  items: TranscriptItem[]
  pendingPermission?: AppState["pendingPermission"]
  running: boolean
  runningStatus: string
  activityGlyph: string
  permissionSelected: number
  selectPermission: (index: number) => void
  expanded: Set<string>
  toggleExpanded: (id: string) => void
  decide: (decision: PermissionDecision) => void
}) {
  return (
    <box flexDirection="column" paddingTop={1} paddingBottom={1} gap={1}>
      <For each={props.items}>
        {(item) => (
          <TimelineItem
            item={item}
            expanded={props.expanded.has(item.id)}
            toggle={() => props.toggleExpanded(item.id)}
          />
        )}
      </For>
      <Show when={props.pendingPermission}>
        {(request) => <PermissionTimelineItem request={request()} selected={props.permissionSelected} select={props.selectPermission} decide={props.decide} />}
      </Show>
      <Show when={props.running && !props.pendingPermission}>
        <AgentWorkingTimelineItem glyph={props.activityGlyph} status={props.runningStatus} />
      </Show>
    </box>
  )
}

function TimelineItem(props: { item: TranscriptItem; expanded: boolean; toggle: () => void }) {
  if (props.item.kind === "user") {
    return (
      <UserPrompt text={props.item.text} />
    )
  }
  if (props.item.kind === "assistant") {
    return (
      <AssistantText text={props.item.text} />
    )
  }
  if (props.item.kind === "tool") {
    return <ToolTimelineItem item={props.item} expanded={props.expanded} toggle={props.toggle} />
  }
  if (props.item.kind === "error") {
    return (
      <TimelineShell marker="!" color={theme.red} label="error">
        <text fg={theme.red} selectable={true} selectionBg={theme.selectionBg} selectionFg={theme.text}>{props.item.text}</text>
      </TimelineShell>
    )
  }
  return null
}

function ToolTimelineItem(props: { item: Extract<TranscriptItem, { kind: "tool" }>; expanded: boolean; toggle: () => void }) {
  const action = createMemo(() => toolActivity(props.item))
  const diff = createMemo(() => diffFromTool(props.item))
  const isWrite = createMemo(() => isWriteTool(props.item.name) || Boolean(diff()))
  const isOpen = createMemo(() => isWrite() || props.expanded)
  const output = createMemo(() => props.item.output?.trim() ?? "")
  const color = createMemo(() => toolColor(props.item))
  const toggle = (event?: { stopPropagation?: () => void }) => {
    event?.stopPropagation?.()
    if (isWrite()) return
    props.toggle()
  }
  return (
    <box width="100%" paddingLeft={1} paddingRight={1}>
      <box
        width="100%"
        flexDirection="column"
        backgroundColor={theme.row}
        border={["top", "bottom"]}
        borderColor={isOpen() ? theme.borderStrong : theme.borderSoft}
      >
        <box
          width="100%"
          flexDirection="row"
          alignItems="center"
          gap={2}
          paddingLeft={1}
          paddingRight={1}
          onMouseUp={toggle}
        >
          <box width={4} alignItems="center" onMouseUp={toggle}>
            <text fg={color()} attributes={TextAttributes.BOLD} selectable={false}>{isWrite() ? "◆" : isOpen() ? "▾" : "▸"}</text>
          </box>
          <text width={12} fg={color()} attributes={TextAttributes.BOLD} selectable={false} onMouseUp={toggle}>TOOL</text>
          <text fg={theme.text} attributes={TextAttributes.BOLD} selectable={false} onMouseUp={toggle}>tool {toolKind(props.item.name)}</text>
          <text fg={theme.dim} selectable={false} onMouseUp={toggle}>-</text>
          <box flexGrow={1} minWidth={0} onMouseUp={toggle}>
            <text fg={theme.text} wrapMode="none" selectable={false}>{action()}</text>
          </box>
          <text fg={theme.dim} selectable={false} onMouseUp={toggle}>{toolMeta(props.item, output())}</text>
        </box>
        <Show when={isOpen()}>
          <box
            flexDirection="column"
            backgroundColor={theme.panelSoft}
            border={["top"]}
            borderColor={isWrite() ? theme.borderStrong : theme.border}
            paddingLeft={1}
            paddingRight={1}
            paddingTop={1}
            paddingBottom={1}
          >
            <Show when={props.item.approval}>
              {(decision) => (
                <box flexDirection="row" gap={1} marginBottom={1}>
                  <text fg={decision() === "deny" ? theme.red : theme.green}>{decision() === "deny" ? "✕" : "✓"}</text>
                  <text fg={decision() === "deny" ? theme.red : theme.green} attributes={TextAttributes.BOLD}>
                    {permissionDecisionText(decision())}
                  </text>
                </box>
              )}
            </Show>
            <Show when={diff()} fallback={<ToolDetails item={props.item} />}>
              {(patch) => (
                <DiffWithHunkReview diff={patch()} path={toolPath(props.item)} />
              )}
            </Show>
            <Show when={output() && !diff()}>
              <box marginTop={1}>
                <code
                  content={output()}
                  filetype={filetype(toolPath(props.item)) ?? "text"}
                  syntaxStyle={syntaxStyle}
                  selectable={true}
                  selectionBg={theme.selectionBg}
                  selectionFg={theme.text}
                />
              </box>
            </Show>
          </box>
        </Show>
      </box>
    </box>
  )
}

function AssistantText(props: { text: string }) {
  return (
    <box width="100%" paddingLeft={2} paddingRight={2}>
      <box width="100%" flexDirection="row">
        <box flexGrow={1} minWidth={0}>
          <markdown content={props.text} fg={theme.text} streaming={true} concealCode={false} syntaxStyle={syntaxStyle} tableOptions={{ selectable: true }} />
        </box>
      </box>
    </box>
  )
}

function UserPrompt(props: { text: string }) {
  return (
    <box width="100%" paddingLeft={2} paddingRight={2}>
      <box
        width="100%"
        backgroundColor="#3a3a3a"
        paddingLeft={1}
        paddingRight={1}
      >
        <text fg={theme.text} selectable={true} selectionBg={theme.selectionBg} selectionFg={theme.text}>{props.text}</text>
      </box>
    </box>
  )
}

function DiffWithHunkReview(props: { diff: string; path?: string }) {
  return (
    <box width="100%" minHeight={0} flexDirection="column">
      <diff
        diff={normalizeDiffForRendering(props.diff)}
        view="split"
        syncScroll={true}
        filetype={filetype(props.path)}
        width="100%"
        minHeight={0}
        wrapMode="word"
        showLineNumbers={true}
        syntaxStyle={syntaxStyle}
        fg={theme.text}
        selectionBg={theme.selectionBg}
        selectionFg={theme.text}
        addedBg={theme.addedBg}
        removedBg={theme.removedBg}
        contextBg={theme.surface}
        addedSignColor={theme.green}
        removedSignColor={theme.red}
        lineNumberFg={theme.muted}
        lineNumberBg={theme.surface}
        addedLineNumberBg={theme.addedBg}
        removedLineNumberBg={theme.removedBg}
      />
    </box>
  )
}


function ToolDetails(props: { item: Extract<TranscriptItem, { kind: "tool" }> }) {
  const input = createMemo(() => prettyJson(props.item.input))
  const output = createMemo(() => props.item.output?.trim() ?? "")
  return (
    <box flexDirection="column" gap={1}>
      <Show when={input()}>
        {(value) => (
          <box flexDirection="column">
            <text fg={theme.dim} selectable={false}>input</text>
            <code content={value()} filetype="json" syntaxStyle={syntaxStyle} selectable={true} selectionBg={theme.selectionBg} selectionFg={theme.text} />
          </box>
        )}
      </Show>
      <Show when={output()}>
        {(value) => (
          <box flexDirection="column">
            <text fg={theme.dim} selectable={false}>output</text>
            <code
              content={value()}
              filetype={filetype(toolPath(props.item)) ?? "text"}
              syntaxStyle={syntaxStyle}
              selectable={true}
              selectionBg={theme.selectionBg}
              selectionFg={theme.text}
            />
          </box>
        )}
      </Show>
    </box>
  )
}

function TimelineShell(props: { marker: string; color: string; label: string; children: unknown }) {
  return (
    <box width="100%" paddingLeft={1} paddingRight={1}>
      <box
        width="100%"
        flexDirection="row"
        gap={2}
        backgroundColor={theme.row}
        border={["top", "bottom"]}
        borderColor={theme.borderSoft}
        paddingLeft={1}
        paddingRight={1}
        paddingTop={0}
        paddingBottom={0}
      >
        <box width={3} alignItems="center">
          <text fg={props.color} selectable={false}>{props.marker}</text>
        </box>
        <text width={12} fg={props.color} attributes={TextAttributes.BOLD} selectable={false}>{props.label.toUpperCase()}</text>
        <box flexGrow={1} minWidth={0} flexDirection="column">
          {props.children}
        </box>
      </box>
    </box>
  )
}

function AgentWorkingTimelineItem(props: { glyph: string; status: string }) {
  return (
    <TimelineShell marker={props.glyph} color={theme.cyan} label="agent">
      <box flexDirection="column">
        <box flexDirection="row" gap={1}>
          <text fg={theme.text} attributes={TextAttributes.BOLD} selectable={false}>{agentActivityText(props.status)}</text>
          <text fg={theme.cyan} selectable={false}>{activityPulse(props.glyph)}</text>
        </box>
        <text fg={theme.dim} selectable={false}>Esc Esc stop · Ctrl+C Ctrl+C quit</text>
      </box>
    </TimelineShell>
  )
}

function PermissionTimelineItem(props: {
  request: NonNullable<AppState["pendingPermission"]>
  selected: number
  select: (index: number) => void
  decide: (decision: PermissionDecision) => void
}) {
  const options: Array<{ decision: PermissionDecision; label: string; shortcut: string; color: string }> = [
    { decision: "allow", shortcut: "1", label: "Yes, allow once", color: theme.blue },
    { decision: "allow_always", shortcut: "2", label: "Yes, allow for this session", color: theme.green },
    { decision: "deny", shortcut: "3", label: "No, deny", color: theme.red },
  ]
  return (
    <TimelineShell marker="?" color={theme.orange} label="approval">
      <box flexDirection="column" gap={1}>
        <text fg={theme.text} attributes={TextAttributes.BOLD} selectable={false}>
          {props.request.toolName} wants permission
        </text>
        <Show
          when={props.request.diff}
          fallback={<code content={props.request.input} filetype="json" syntaxStyle={syntaxStyle} selectable={true} selectionBg={theme.selectionBg} selectionFg={theme.text} />}
        >
          {(patch) => (
            <box backgroundColor={theme.panelSoft} border={["top", "bottom"]} borderColor={theme.borderStrong} paddingLeft={1} paddingRight={1} paddingTop={1} paddingBottom={1}>
              <DiffWithHunkReview diff={normalizeUnifiedPatch(props.request.filepath ?? "file", patch())} path={props.request.filepath} />
            </box>
          )}
        </Show>
        <box flexDirection="column" gap={0}>
          <For each={options}>
            {(option, index) => {
              const selected = () => props.selected === index()
              return (
                <box
                  width="100%"
                  flexDirection="row"
                  gap={1}
                  paddingLeft={1}
                  backgroundColor={selected() ? theme.paletteSelected : theme.row}
                  onMouseUp={() => {
                    props.select(index())
                    props.decide(option.decision)
                  }}
                >
                  <text width={3} fg={selected() ? theme.cyan : option.color} attributes={selected() ? TextAttributes.BOLD : undefined} selectable={false}>{selected() ? "›" : " "}{option.shortcut}</text>
                  <text fg={selected() ? theme.text : option.color} attributes={selected() ? TextAttributes.BOLD : undefined} selectable={false}>{option.label}</text>
                </box>
              )
            }}
          </For>
          <text fg={theme.dim} selectable={false}>↑/↓ move · enter choose · 1/2/3 quick</text>
        </box>
      </box>
    </TimelineShell>
  )
}

function Composer(props: {
  state: AppState
  provider: string
  model: string
  mode: EffortValue
  agent: string
  inputRef: (ref: TextareaRenderable) => void
  draft: () => string
  setDraft: (value: string) => void
  submit: () => void
  palette: () => PaletteKind
  paletteItems: () => readonly PaletteItem[]
  selected: () => number
  moveSelection: (delta: number) => void
  acceptPalette: (insertDirectory?: boolean) => void
  choosePalette: (index: number) => void
  openPalette: (kind: PaletteKind) => void
  insertPromptNewline: () => void
  navigatePromptHistory: (direction: HistoryDirection) => boolean
  notice: ComposerNotice
  pasteFromClipboard: () => Promise<void>
}) {
  let textarea!: TextareaRenderable
  const showActivity = () => Boolean(props.state.pendingPermission) || props.notice.tone !== "muted"
  return (
    <box flexShrink={0} flexDirection="column" paddingLeft={2} paddingRight={2} paddingBottom={1}>
      <Show when={props.palette()}>
        {(kind) => <Palette kind={kind()} items={props.paletteItems()} selected={props.selected()} choose={props.choosePalette} />}
      </Show>
      <Show when={showActivity()}>
        <box
          flexDirection="row"
          alignItems="center"
          gap={1}
          marginBottom={1}
        >
          <box
            flexDirection="row"
            alignItems="center"
            gap={1}
            backgroundColor={theme.surface2}
            border
            borderStyle="rounded"
            borderColor={noticeColor(props.notice)}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={noticeColor(props.notice)} attributes={TextAttributes.BOLD}>
              {props.notice.tone === "red" ? "!" : "•"}
            </text>
            <text fg={noticeColor(props.notice)} attributes={props.notice.tone === "cyan" || props.notice.tone === "red" ? TextAttributes.BOLD : undefined}>
              {props.notice.text}
            </text>
          </box>
        </box>
      </Show>
      <box
        width="100%"
        flexDirection="column"
        backgroundColor={theme.surface3}
        border
        borderStyle="rounded"
        borderColor={props.state.pendingPermission ? theme.orange : theme.railActive}
        paddingLeft={1}
        paddingRight={1}
        paddingTop={1}
        paddingBottom={1}
      >
        <box width="100%" flexDirection="row" alignItems="center" gap={1}>
          <text fg={theme.cyan}>›</text>
          <textarea
            width="100%"
            minHeight={1}
            maxHeight={5}
            placeholder={props.state.pendingPermission ? "approval required: press 1, 2, or 3" : props.state.running ? "agent running..." : "Ask INDUCTOR..."}
            placeholderColor={theme.dim}
            textColor={theme.text}
            focusedTextColor={theme.text}
            focusedBackgroundColor={theme.surface3}
            cursorColor={theme.cyan}
            selectionBg={theme.selectionBg}
            selectionFg={theme.text}
            keyBindings={[{ name: "j", ctrl: true, action: "newline" }]}
            onContentChange={() => props.setDraft(textarea.plainText)}
            onSubmit={props.submit}
            onPaste={async (event: { bytes?: Uint8Array; preventDefault(): void }) => {
              const text = decodePasteBytes(event.bytes).replace(/\r\n/g, "\n").replace(/\r/g, "\n")
              if (!text.trim()) {
                event.preventDefault()
                await props.pasteFromClipboard()
                return
              }

              event.preventDefault()
              const next = `${textarea.plainText}${text}`
              textarea.setText(next)
              props.setDraft(next)
            }}
            onKeyDown={(event: { key?: string; name?: string; ctrl?: boolean; meta?: boolean; super?: boolean; ctrlKey?: boolean; metaKey?: boolean; preventDefault(): void; stopPropagation?: () => void; sequence?: string }) => {
              const key = event.key ?? event.name
              const normalized = key?.toLowerCase()
              const ctrl = Boolean(event.ctrlKey || event.ctrl)
              const meta = Boolean(event.metaKey || event.meta || event.super)
              const permissionNav = key === "ArrowUp" || key === "up" || key === "ArrowDown" || key === "down" || key === "Enter" || key === "enter" || key === "return"
              if (props.state.pendingPermission && permissionNav) return
              if (props.palette() && (key === "ArrowUp" || key === "up")) {
                event.preventDefault()
                event.stopPropagation?.()
                props.moveSelection(-1)
                return
              }
              if (props.palette() && (key === "ArrowDown" || key === "down")) {
                event.preventDefault()
                event.stopPropagation?.()
                props.moveSelection(1)
                return
              }
              if (props.palette() && (key === "Enter" || key === "enter" || key === "return")) {
                event.preventDefault()
                event.stopPropagation?.()
                props.acceptPalette(Boolean(meta || ctrl))
                return
              }
              if (!props.palette() && (meta || ctrl) && normalized === "v") {
                event.preventDefault()
                event.stopPropagation?.()
                void props.pasteFromClipboard()
                return
              }
              if (!props.palette() && ((ctrl && (normalized === "j" || normalized === "linefeed")) || key === "\n" || event.sequence === "\n")) {
                event.preventDefault()
                event.stopPropagation?.()
                props.insertPromptNewline()
                return
              }
              if (!props.palette() && (key === "ArrowUp" || key === "up")) {
                if (props.navigatePromptHistory(-1)) {
                  event.preventDefault()
                  event.stopPropagation?.()
                }
                return
              }
              if (!props.palette() && (key === "ArrowDown" || key === "down")) {
                if (props.navigatePromptHistory(1)) {
                  event.preventDefault()
                  event.stopPropagation?.()
                }
                return
              }
              if (!props.palette() && (key === "Enter" || key === "enter" || key === "return")) {
                event.preventDefault()
                event.stopPropagation?.()
                props.submit()
                return
              }
              if (!props.palette() && key === "Tab") {
                event.preventDefault()
                props.openPalette("agents")
                return
              }
              if (!props.palette() && ctrl && normalized === "p") {
                event.preventDefault()
                props.openPalette("commands")
                return
              }
              if (!props.palette() && ctrl && normalized === "t") {
                event.preventDefault()
                props.openPalette("modes")
                return
              }
            }}
            ref={(ref: TextareaRenderable) => {
              textarea = ref
              props.inputRef(ref)
              queueMicrotask(() => ref.focus())
            }}
          />
        </box>
      </box>
    </box>
  )
}

function Palette(props: {
  kind: PaletteKind
  items: readonly PaletteItem[]
  selected: number
  choose: (index: number) => void
}) {
  return (
    <box
      width="100%"
      backgroundColor={theme.palette}
      border
      borderStyle="rounded"
      borderColor={theme.border}
      paddingLeft={2}
      paddingRight={2}
      paddingTop={1}
      paddingBottom={1}
      marginBottom={1}
    >
      <For each={props.items}>
        {(item, index) => {
          const selected = () => index() === props.selected
          return (
            <box
              flexDirection="row"
              backgroundColor={selected() ? theme.paletteSelected : theme.palette}
              paddingLeft={1}
              onMouseUp={() => props.choose(index())}
            >
              <text width={18} fg={selected() ? theme.cyan : theme.text} attributes={selected() ? TextAttributes.BOLD : undefined}>
                {paletteItemLabel(item)}
              </text>
              <text fg={selected() ? theme.text : theme.muted}>
                {paletteItemDescription(item)}
              </text>
            </box>
          )
        }}
      </For>
    </box>
  )
}

function paletteItemLabel(item: PaletteItem) {
  return "label" in item ? item.label : item.name
}

function paletteItemDescription(item: PaletteItem) {
  if ("efforts" in item) return item.group
  if ("description" in item) return item.description
  return ""
}

function TelemetrySidebar(props: {
  state: AppState
  provider: string
  model: string
  workspace: string
  worktreePath: string
  contextPercent: number
  mode: EffortValue
  branch: string
  openModifiedFile: (file: ModifiedFile) => void
}) {
  return (
    <box
      width={TELEMETRY_SIDEBAR_WIDTH}
      height="100%"
      flexShrink={0}
      backgroundColor={theme.panelSoft}
      border
      borderStyle="rounded"
      borderColor={theme.border}
      paddingTop={1}
      paddingLeft={2}
      paddingRight={2}
      paddingBottom={1}
    >
      <scrollbox flexGrow={1} scrollAcceleration={scrollAcceleration} verticalScrollbarOptions={{ visible: false }}>
        <box flexDirection="column" gap={2}>
          <text fg={theme.text} attributes={TextAttributes.BOLD}>{props.state.title}</text>
          <box flexDirection="column">
            <text fg={theme.cyan}>CONTEXT</text>
            <text fg={theme.muted}>{props.state.tokens.toLocaleString()} tokens</text>
            <text fg={theme.muted}>{props.contextPercent}% used</text>
            <box width={TELEMETRY_PROGRESS_WIDTH} height={1} backgroundColor={theme.progressTrack} marginTop={1}>
              <box width={Math.max(1, Math.floor((props.contextPercent / 100) * TELEMETRY_PROGRESS_WIDTH))} height={1} backgroundColor={theme.progress} />
            </box>
            <text fg={theme.muted}>{money.format(props.state.costUsd)} spent</text>
          </box>
          <SectionDivider />
          <WorktreePathSection path={props.worktreePath} />
          <SectionDivider />
          <ModifiedFiles files={props.state.modifiedFiles} openFile={props.openModifiedFile} />
        </box>
      </scrollbox>
      <box flexShrink={0} flexDirection="column" gap={1}>
        <text fg={theme.muted}>{truncateRight(`${shortWorkspace(props.workspace)}:${props.branch}`, TELEMETRY_FOOTER_WIDTH)}</text>
        <box flexDirection="row" gap={1}>
          <text fg={theme.green}>•</text>
          <text fg={theme.text} attributes={TextAttributes.BOLD}>Inductor</text>
          <text fg={theme.muted}>0.1.0</text>
        </box>
      </box>
    </box>
  )
}

function SectionDivider() {
  return <box width="100%" height={1} border={["top"]} borderColor={theme.borderSoft} />
}

function WorktreePathSection(props: { path: string }) {
  // Show the worktree's full path. It must never be truncated, so we wrap it
  // across as many lines as needed (no "..." ellipsis even for long paths).
  return (
    <box flexDirection="column" gap={1}>
      <text fg={theme.cyan}>WORKTREE PATH</text>
      <text fg={theme.muted} wrapMode="char" selectable={true}>{props.path}</text>
    </box>
  )
}

function ModifiedFiles(props: { files: ModifiedFile[]; openFile: (file: ModifiedFile) => void }) {
  return (
    <box flexDirection="column" gap={1}>
      <text fg={theme.cyan}>MODIFIED FILES</text>
      <Show when={props.files.length > 0} fallback={<text fg={theme.dim}>No changes yet</text>}>
        <For each={props.files}>
          {(file) => (
            <box
              flexDirection="row"
              justifyContent="space-between"
              gap={1}
              onMouseUp={() => props.openFile(file)}
            >
              <text
                fg={theme.muted}
                attributes={TextAttributes.UNDERLINE}
                wrapMode="none"
                selectable={false}
              >
                {truncateLeft(file.file, TELEMETRY_FILE_WIDTH)}
              </text>
              <box flexShrink={0} flexDirection="row" gap={1}>
                <Show when={file.additions > 0}><text fg={theme.cyan}>+{file.additions}</text></Show>
                <Show when={file.deletions > 0}><text fg={theme.red}>-{file.deletions}</text></Show>
              </box>
            </box>
          )}
        </For>
      </Show>
    </box>
  )
}

function defaultModel(provider: string) {
  if (provider === "copilot") return "gpt-4.1"
  return provider === "codex" ? "gpt-5.5" : "sonnet"
}

function copilotModelChoice(model: ProviderModel): ModelChoice {
  return {
    group: "GitHub Copilot",
    provider: "copilot",
    model: model.id,
    label: `Copilot ${model.display_name || model.id}`,
    effortName: "Reasoning",
    efforts: ["low", "medium", "high", "xhigh"],
  }
}

function selectedModelChoice(providerValue = "", modelValue = "") {
  const activeProvider = providerValue || undefined
  const activeModel = modelValue || undefined
  return modelChoices.find((choice) => choice.provider === activeProvider && choice.model === activeModel)
}

function effortChoices(choice?: ModelChoice): EffortChoice[] {
  const efforts: EffortValue[] = choice?.efforts.length ? choice.efforts : ["low", "medium", "high"]
  const effortName = choice?.effortName ?? "reasoning"
  return efforts.map((value) => ({
    name: value,
    label: effortDisplay(value, choice),
    value,
    description: value === "ultracode" ? "xhigh + workflows" : effortName,
  }))
}

function coerceEffortForModel(value: EffortValue, choice: ModelChoice) {
  if (choice.efforts.includes(value)) return value
  if (choice.efforts.includes("medium")) return "medium"
  return choice.efforts[0] ?? value
}

function effortDisplay(value: EffortValue, choice?: ModelChoice) {
  if (choice?.effortLabels?.[value]) return choice.effortLabels[value]
  if (choice?.provider === "codex" || choice?.provider === "copilot") {
    if (value === "xhigh") return "Extra High"
    return titleCase(value)
  }
  if (choice?.provider === "claude") {
    if (value === "low") return "Low (Faster)"
    if (value === "max") return "Max (Smarter)"
    if (value === "ultracode") return "Ultracode"
    return value
  }
  return value
}

function backendEffort(value: EffortValue) {
  return value === "ultracode" ? "xhigh" : value
}

function titleCase(value: string) {
  return value.slice(0, 1).toUpperCase() + value.slice(1)
}

function visibleStderr(text: string) {
  return text
    .split(/\r?\n/)
    .map((line) => line.trimEnd())
    .filter((line) => line.length > 0)
    .filter((line) => !line.startsWith("persisted_session_id:"))
    .filter((line) => !line.startsWith("workspace_db:"))
    .join("\n")
}

function providerLabel(provider: string) {
  if (provider === "copilot") return "Copilot"
  return provider === "codex" ? "OpenAI" : "Claude"
}

function sessionProvider(value: unknown) {
  if (typeof value === "string") return value
  if (value && typeof value === "object" && "0" in value) {
    const tuple = value as { 0?: unknown }
    return typeof tuple[0] === "string" ? tuple[0] : undefined
  }
  return undefined
}

function shortProviderModel(provider: string, model: string) {
  return `${providerLabel(provider)} · ${shortModel(model)}`
}

function modelDisplay(provider: string, model: string) {
  const choice = selectedModelChoice(provider, model)
  if (choice) return choice.label
  return `${providerLabel(provider)} ${model}`
}

function runningGlyph(now: number) {
  const frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
  return frames[Math.floor(now / 90) % frames.length] ?? "⠋"
}

function activityPulse(glyph: string) {
  const frames = ["   ", ".  ", ".. ", "..."]
  const seed = glyph.codePointAt(0) ?? 0
  return frames[seed % frames.length] ?? "..."
}

function agentActivityText(status: string) {
  if (status === "running_tools") return "running tools"
  if (status === "waiting_for_permission") return "waiting for approval"
  if (status === "streaming") return "writing response"
  if (status === "running" || !status || status === "idle") return "agent is working"
  return status.replaceAll("_", " ")
}

function toolLabel(name: string) {
  if (name === "read_file") return "Read"
  if (name === "write_file") return "Write"
  if (name === "edit_file") return "Edit"
  if (name === "bash") return "Run"
  if (name.includes("grep")) return "Search"
  return name.replaceAll("_", " ")
}

function toolKind(name: string) {
  const lower = name.toLowerCase()
  if (lower === "read_file" || lower === "read") return "read file"
  if (lower === "write_file" || lower === "write") return "write file"
  if (lower === "edit_file" || lower.includes("edit")) return "edit file"
  if (lower.startsWith("apply_patch")) return "apply patch"
  if (lower === "bash" || lower.includes("shell")) return "bash"
  if (lower.includes("grep") || lower.includes("search")) return "search"
  if (lower.includes("list") || lower.includes("ls")) return "list dir"
  return lower.replaceAll("_", " ")
}

function toolColor(item: Extract<TranscriptItem, { kind: "tool" }>) {
  if (item.approval === "deny") return theme.red
  if (item.approval === "allow" || item.approval === "allow_always") return theme.green
  if (item.status === "error") return theme.red
  if (item.status === "done") return theme.blue
  return theme.green
}

function toolMeta(item: Extract<TranscriptItem, { kind: "tool" }>, output: string) {
  const diff = diffFromTool(item)
  if (diff) {
    const changes = diffStats(diff)
    return `${changes.additions}+ ${changes.deletions}-`
  }
  if (output) return `${output.split(/\r?\n/).length} lines`
  if (item.status === "running") return "running"
  return ""
}

function toolActivity(item: Extract<TranscriptItem, { kind: "tool" }>) {
  const path = toolPath(item)
  const kind = toolKind(item.name)
  const description = toolDescription(item)
  const command = commandFromTool(item)
  const query = queryFromTool(item)
  if (kind === "bash") {
    const commandText = command ?? description ?? "command"
    return `${toolVerb(item.status, "bash")} ${truncateRight(commandPurpose(commandText), 118)}`
  }
  if (kind === "read file") {
    return `${toolVerb(item.status, "read")} file ${truncateLeft(path ?? description ?? "file", 82)}`
  }
  if (kind === "write file" || kind === "edit file" || kind === "apply patch") {
    return `${toolVerb(item.status, "write")} ${truncateLeft(path ?? description ?? "patch", 88)}`
  }
  if (kind === "search") {
    return `${toolVerb(item.status, "search")} ${truncateRight(query ?? description ?? path ?? "workspace", 96)}`
  }
  if (kind === "list dir") {
    return `${toolVerb(item.status, "list")} ${truncateLeft(path ?? description ?? "workspace", 88)}`
  }
  if (path) return `${toolVerb(item.status, "generic")} ${truncateLeft(path, 92)}`
  return description ? truncateRight(description, 110) : toolLabel(item.name)
}

function toolVerb(status: string, kind: "bash" | "read" | "write" | "search" | "list" | "generic") {
  if (status === "error") return "failed"
  if (kind === "bash") return status === "running" ? "running" : "ran"
  if (kind === "read") return status === "running" ? "reading" : "read"
  if (kind === "write") return status === "running" ? "updating" : "updated"
  if (kind === "search") return status === "running" ? "searching" : "searched"
  if (kind === "list") return status === "running" ? "listing" : "listed"
  return status === "running" ? "using" : "used"
}

function permissionDecisionText(decision: PermissionDecision) {
  if (decision === "allow") return "Allowed once"
  if (decision === "allow_always") return "Allowed for this session"
  return "Denied"
}

function commandPurpose(command: string) {
  const trimmed = command.trim()
  if (/(\bnpm\b|\bbun\b|\byarn\b|\bpnpm\b).*(dev|start|serve)|cargo run|python .*server|uvicorn|next dev/i.test(trimmed)) {
    return `server command: ${trimmed}`
  }
  if (/test|cargo test|pytest|vitest|jest|bun test/i.test(trimmed)) return `tests: ${trimmed}`
  if (/build|cargo build|tsc|typecheck/i.test(trimmed)) return `build check: ${trimmed}`
  return trimmed
}

function diffStats(diff: string) {
  let additions = 0
  let deletions = 0
  for (const line of diff.split(/\r?\n/)) {
    if (line.startsWith("+++") || line.startsWith("---")) continue
    if (line.startsWith("+")) additions += 1
    if (line.startsWith("-")) deletions += 1
  }
  return { additions, deletions }
}

function isWriteTool(name: string) {
  const lower = name.toLowerCase()
  return lower === "write_file" || lower === "edit_file" || lower === "multi_edit" || lower.startsWith("apply_patch") || lower.includes("edit") || lower.includes("write") || lower.includes("patch")
}

function toolPath(item: Extract<TranscriptItem, { kind: "tool" }>) {
  const json = parseMaybeJson(item.input)
  return stringField(json, ["path", "filepath", "file_path", "target", "filename"]) ?? patchPathFromToolInput(json)
}

function commandFromTool(item: Extract<TranscriptItem, { kind: "tool" }>) {
  const json = parseMaybeJson(item.input)
  return stringField(json, ["cmd", "command"])
}

function queryFromTool(item: Extract<TranscriptItem, { kind: "tool" }>) {
  const json = parseMaybeJson(item.input)
  return stringField(json, ["query", "pattern", "search", "regex"])
}

function toolDescription(item: Extract<TranscriptItem, { kind: "tool" }>) {
  const json = parseMaybeJson(item.input)
  return stringField(json, ["description", "summary", "reason", "message"])
}

function diffFromTool(item: Extract<TranscriptItem, { kind: "tool" }>) {
  if (item.diff) return item.diff
  const json = parseMaybeJson(item.input)
  const explicitDiff = stringField(json, ["diff"])
  const path = toolPath(item) ?? "file"
  if (explicitDiff) return normalizeUnifiedPatch(path, explicitDiff)
  const patchValue = json?.patch
  if (typeof patchValue === "string") return normalizeUnifiedPatch(path, patchValue)
  if (Array.isArray(patchValue)) {
    const diffs = patchValue
      .map((file) => (file && typeof file === "object" && typeof (file as Record<string, unknown>).diff === "string" ? String((file as Record<string, unknown>).diff) : undefined))
      .filter((diff): diff is string => Boolean(diff?.trim()))
    if (diffs.length > 0) return diffs.join("\n")
  }
  const operations = Array.isArray(json?.operations) ? json.operations : []
  if (operations.length > 0) {
    const diffs = operations
      .map((operation) => diffFromPatchOperation(operation))
      .filter((diff): diff is string => Boolean(diff?.trim()))
    if (diffs.length > 0) return diffs.join("\n")
  }
  const edits = editsArray(json)
  if (edits.length > 0) {
    const diffs = edits
      .map((edit) => createUnifiedPatchFromContent(path, edit.old, edit.new))
      .filter((diff): diff is string => Boolean(diff))
    if (diffs.length > 0) return diffs.join("\n")
  }
  const oldText = stringField(json, ["old", "old_text", "before"])
  const newText = stringField(json, ["new", "new_text", "content", "after"])
  if (!oldText && !newText) return undefined
  return createUnifiedPatchFromContent(path, oldText ?? "", newText ?? "")
}

function diffFromPatchOperation(operation: unknown) {
  if (!operation || typeof operation !== "object") return undefined
  const op = operation as Record<string, unknown>
  const type = stringField(op, ["type"])
  if (type === "rename") {
    const from = stringField(op, ["from"])
    const to = stringField(op, ["to"])
    const diffs = [
      from ? createUnifiedPatchFromContent(from, `renamed to ${to ?? "new path"}\n`, "") : undefined,
      to ? createUnifiedPatchFromContent(to, "", `renamed from ${from ?? "old path"}\n`) : undefined,
    ].filter((diff): diff is string => Boolean(diff))
    return diffs.length > 0 ? diffs.join("\n") : undefined
  }
  const path = stringField(op, ["path", "filepath", "file_path", "target", "filename"]) ?? "file"
  const edits = editsArray(op)
  if (edits.length > 0) {
    const diffs = edits
      .map((edit) => createUnifiedPatchFromContent(path, edit.old, edit.new))
      .filter((diff): diff is string => Boolean(diff))
    return diffs.length > 0 ? diffs.join("\n") : undefined
  }
  const oldText = stringField(op, ["old", "old_text", "before"])
  const newText = stringField(op, ["new", "new_text", "content", "after"])
  return oldText || newText ? createUnifiedPatchFromContent(path, oldText ?? "", newText ?? "") : undefined
}

function editsArray(value: Record<string, unknown> | undefined) {
  const edits = Array.isArray(value?.edits) ? value.edits : []
  return edits
    .map((edit) => {
      if (!edit || typeof edit !== "object") return undefined
      const item = edit as Record<string, unknown>
      return { old: stringField(item, ["old"]) ?? "", new: stringField(item, ["new"]) ?? "" }
    })
    .filter((edit): edit is { old: string; new: string } => Boolean(edit))
}

function patchPathFromToolInput(json: Record<string, unknown> | undefined) {
  const patch = json?.patch
  if (typeof patch === "string") return patchFilesFromUnifiedPatch(patch)[0]?.path
  if (Array.isArray(patch)) {
    const first = patch.find((file) => file && typeof file === "object" && typeof (file as Record<string, unknown>).path === "string") as Record<string, unknown> | undefined
    return typeof first?.path === "string" ? first.path : undefined
  }
  const diff = stringField(json, ["diff"])
  return diff ? patchFilesFromUnifiedPatch(diff)[0]?.path : undefined
}

function prettyJson(value: string) {
  const parsed = parseMaybeJson(value)
  if (!parsed) return value
  return JSON.stringify(parsed, null, 2)
}

function parseMaybeJson(value: string) {
  try {
    return JSON.parse(value) as Record<string, unknown>
  } catch {
    return undefined
  }
}

function stringField(value: Record<string, unknown> | undefined, names: string[]) {
  if (!value) return undefined
  for (const name of names) {
    const candidate = value[name]
    if (typeof candidate === "string" && candidate.length > 0) return candidate
  }
  return undefined
}

function codeFence(content: string, lang: string) {
  return `\`\`\`${lang}\n${content}\n\`\`\``
}

function decodePasteBytes(bytes?: Uint8Array) {
  if (!bytes?.length) return ""
  return new TextDecoder().decode(bytes)
}

function filetype(path?: string) {
  if (!path) return undefined
  if (path.endsWith(".rs")) return "rust"
  if (path.endsWith(".ts") || path.endsWith(".tsx")) return "typescript"
  if (path.endsWith(".js") || path.endsWith(".jsx")) return "javascript"
  if (path.endsWith(".py")) return "python"
  if (path.endsWith(".md")) return "markdown"
  if (path.endsWith(".toml")) return "toml"
  if (path.endsWith(".json")) return "json"
  if (path.endsWith(".sh")) return "bash"
  return undefined
}

function shortWorkspace(path: string) {
  const parts = path.split("/").filter(Boolean)
  if (parts.length <= 2) return path
  return `~/${parts.slice(-2).join("/")}`
}

function shortModel(value: string) {
  return value.replace("claude-", "").replace("gpt-", "gpt ")
}

function truncateLeft(value: string, max: number) {
  if (value.length <= max) return value
  return `...${value.slice(-(max - 3))}`
}

function truncateRight(value: string, max: number) {
  if (value.length <= max) return value
  return `${value.slice(0, max - 3)}...`
}

function formatElapsed(ms: number) {
  const total = Math.max(0, Math.floor(ms / 1000))
  const minutes = Math.floor(total / 60)
  const seconds = total % 60
  if (minutes < 60) return `${minutes}m ${seconds}s`
  const hours = Math.floor(minutes / 60)
  return `${hours}h ${minutes % 60}m`
}

function clockElapsed(value: string) {
  const match = value.match(/^(\d+)m (\d+)s$/)
  if (!match) return value
  return `${match[1].padStart(2, "0")}:${match[2].padStart(2, "0")}`
}
