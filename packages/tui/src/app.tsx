/** @jsxImportSource @opentui/solid */
import { MacOSScrollAccel, SyntaxStyle, TextAttributes, TextareaRenderable, type KeyEvent } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs"
import path from "node:path"
import { For, Show, createMemo, createSignal, onCleanup, onMount } from "solid-js"
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
import { listWorkspaceSessions, showWorkspaceSession, startBackendTurn, type BackendOptions, type BackendRun, type PermissionDecision, type StoredSession } from "./backend"
import { readClipboard } from "./clipboard"
import { createUnifiedPatchFromContent } from "./diff_patch"
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

export type AppProps = BackendOptions & {
  exitApp(): void
  registerCtrlCHandler(handler: (() => void) | undefined): void
}

type EffortValue = "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultracode"
type PaletteKind = "commands" | "models" | "agents" | "modes" | "files" | undefined
type CommandAction = "agents" | "clear" | "connect" | "exit" | "help" | "mode" | "model" | "new" | "review" | "sessions"
type Command = { name: string; description: string; action: CommandAction }
type ModelChoice = { provider: string; model: string; label: string; group: string; effortName: string; efforts: EffortValue[]; effortLabels?: Partial<Record<EffortValue, string>> }
type EffortChoice = { name: string; label: string; description: string; value: EffortValue }
type AgentChoice = { name: string; description: string }
type PaletteItem = Command | ModelChoice | EffortChoice | AgentChoice | FileChoice
type StopIntent = "interrupt" | "exit"
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
const DIFF_VIEWER_MIN_HEIGHT = 8
const DIFF_VIEWER_MAX_HEIGHT = 28

const commands: Command[] = [
  { name: "/agents", description: "Switch agent", action: "agents" },
  { name: "/connect", description: "Connect provider", action: "connect" },
  { name: "/effort", description: "Switch reasoning effort", action: "mode" },
  { name: "/fast", description: "Switch reasoning effort", action: "mode" },
  { name: "/help", description: "Show shortcuts", action: "help" },
  { name: "/model", description: "Switch model", action: "model" },
  { name: "/new", description: "New session", action: "new" },
  { name: "/review", description: "Review changes", action: "review" },
  { name: "/sessions", description: "Open sessions", action: "sessions" },
  { name: "/clear", description: "Clear transcript", action: "clear" },
  { name: "/exit", description: "Exit app", action: "exit" },
]

const modelChoices: ModelChoice[] = [
  { group: "Claude", provider: "claude", model: "sonnet", label: "Claude Sonnet", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "Claude", provider: "claude", model: "fable", label: "Fable", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "Claude", provider: "claude", model: "opus", label: "Opus (1M context)", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "Claude", provider: "claude", model: "haiku", label: "Haiku", effortName: "Claude effort", efforts: ["low", "medium", "high", "xhigh", "max", "ultracode"] },
  { group: "OpenAI", provider: "codex", model: "gpt-5.5", label: "GPT-5.5", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.4", label: "GPT-5.4", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.4-mini", label: "GPT-5.4-Mini", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
]

const agentChoices: AgentChoice[] = [
  { name: "Build", description: "Implement and verify changes" },
  { name: "Review", description: "Inspect code and risks first" },
  { name: "Plan", description: "Outline before editing" },
]

const money = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" })
const scrollAcceleration = new MacOSScrollAccel({ maxMultiplier: 2.8 })
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
  let currentRun: BackendRun | undefined
  let replacingPrompt = false
  let stoppingRun = false
  let exitAfterStop = false
  let lastCtrlCAt = 0
  let stopArmTimer: ReturnType<typeof setTimeout> | undefined
  let forceStopTimer: ReturnType<typeof setTimeout> | undefined
  const startedAt = Date.now()
  const [state, setState] = createStore(createInitialState())
  const [draft, setDraft] = createSignal("")
  const [stopArmed, setStopArmed] = createSignal<StopIntent>()
  const [notice, setNotice] = createSignal<ComposerNotice>()
  const [permissionSelected, setPermissionSelected] = createSignal(0)
  const promptHistoryPath = path.join(props.workspace, ".inductor", "prompt-history.json")
  const [promptHistory, setPromptHistory] = createSignal<PromptHistoryState>({ entries: loadPromptHistoryFile(promptHistoryPath), draft: "" })
  const [palette, setPalette] = createSignal<PaletteKind>()
  const [selected, setSelected] = createSignal(0)
  const [mention, setMention] = createSignal<MentionState>()
  const [pasteCount, setPasteCount] = createSignal(0)
  const [promptImages, setPromptImages] = createSignal<PromptImageAttachment[]>([])
  const [mode, setMode] = createSignal<EffortValue>("medium")
  const [agent, setAgent] = createSignal("Build")
  const [model, setModel] = createSignal(props.model ?? defaultModel(props.provider))
  const [provider, setProvider] = createSignal(props.provider)
  const [sessionId, setSessionId] = createSignal<string>()
  const [sessions, setSessions] = createSignal<StoredSession[]>([])
  const [sessionListStatus, setSessionListStatus] = createSignal("")
  const [expanded, setExpanded] = createSignal<Set<string>>(new Set())
  const [now, setNow] = createSignal(Date.now())
  const dimensions = useTerminalDimensions()
  const contextPercent = createMemo(() => Math.min(99, Math.round((state.tokens / 200_000) * 100)))
  const hasTranscript = createMemo(() => state.transcript.length > 0)
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
    if (palette() === "models") return modelChoices
    if (palette() === "agents") return agentChoices
    if (palette() === "modes") return effortChoices(selectedModelChoice(provider(), model()))
    return commandItems()
  })

  const timer = setInterval(() => setNow(Date.now()), 1000)
  const composerNotice = createMemo(() => notice() ?? defaultComposerNotice(state.status, state.running, state.pendingPermission))
  onMount(() => {
    props.registerCtrlCHandler(handleCtrlC)
    void refreshSessions()
  })
  onCleanup(() => {
    props.registerCtrlCHandler(undefined)
    clearInterval(timer)
    clearStopArmTimer()
    clearForceStopTimer()
    if (currentRun) {
      const run = currentRun
      currentRun = undefined
      run.kill()
      void run.exited.catch(() => undefined)
    }
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
    if (state.pendingPermission && handlePermissionKey(event)) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (isEscape(event) && (state.running || currentRun)) {
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
    if (!visiblePrompt || state.running) return
    if (palette()) {
      acceptPalette()
      return
    }

    input.setText("")
    setDraft("")
    recordHistory(visiblePrompt)
    setPromptImages([])
    setPalette(undefined)
    stoppingRun = false
    exitAfterStop = false
    clearForceStopTimer()
    disarmStopWarning()
    setNotice(undefined)
    setState(produce((next) => Object.assign(next, addUserMessage(next, visiblePrompt))))
    currentRun = startBackendTurn(prompt, {
      ...props,
      provider: provider(),
      model: model(),
      sessionId: sessionId(),
      effort: backendEffort(mode()),
    }, {
      onEvent(event) {
        if (event.session_id) setSessionId(event.session_id)
        if (event.type === "permission_request") setPermissionSelected(0)
        if (stoppingRun) {
          if (event.type === "result" || event.type === "error") {
            setState(produce((next) => Object.assign(next, markAgentStopped(next))))
          }
          return
        }
        setState(produce((next) => Object.assign(next, applySessionEvent(next, event))))
      },
      onStderr(text) {
        const lines = visibleStderr(text)
        if (!lines) return
        setNotice({ text: truncateRight(lines.replace(/\s+/g, " "), 120), tone: "muted" })
      },
      onExit(code) {
        clearForceStopTimer()
        currentRun = undefined
        if (exitAfterStop) {
          props.exitApp()
          return
        }
        if (stoppingRun) {
          stoppingRun = false
          setNotice({ text: "stopped agent", tone: "red" })
          setState(produce((next) => Object.assign(next, markAgentStopped(next))))
          void refreshSessions()
          return
        }
        setState("running", false)
        setState("status", code === 0 ? "idle" : `exited ${code ?? "unknown"}`)
        void refreshSessions()
      },
    })
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
    runCommand(item as Command)
  }

  function closePalette() {
    setPalette(undefined)
    setMention(undefined)
    setSelected(0)
    input.setText("")
    setDraft("")
    queueMicrotask(() => input.focus())
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
    if (command.action === "agents") {
      openPalette("agents")
      return
    }
    if (command.action === "mode") {
      openPalette("modes")
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

  function startNewSession() {
    setSessionId(undefined)
    setState(createInitialState())
    setExpanded(new Set<string>())
    setNotice(undefined)
    queueMicrotask(() => input?.focus())
  }

  async function refreshSessions() {
    try {
      const next = await listWorkspaceSessions(props)
      setSessions(next)
      setSessionListStatus("")
    } catch (error) {
      setSessionListStatus(error instanceof Error ? error.message : "Could not load sessions")
    }
  }

  async function loadSession(id: string) {
    if (state.running || currentRun) {
      setNotice({ text: "Stop the running agent before switching sessions", tone: "cyan" })
      return
    }
    try {
      const detail = await showWorkspaceSession(props, id)
      setSessionId(id)
      setProvider(sessionProvider(detail.session.provider_id) ?? provider())
      setModel(detail.session.model || model())
      setState(loadStoredSession(detail))
      setExpanded(new Set<string>())
      setNotice({ text: "session loaded", tone: "muted" })
      queueMicrotask(() => input?.focus())
    } catch (error) {
      setNotice({ text: error instanceof Error ? error.message : "Could not load session", tone: "red" })
    }
  }

  function decide(decision: PermissionDecision) {
    const request = state.pendingPermission
    if (!request || !currentRun) return
    currentRun.respond(request.requestId, decision)
    setPermissionSelected(0)
    setState(produce((next) => Object.assign(next, applyPermissionDecision(next, decision))))
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
    if (stopArmed() === "exit") {
      if (state.running || currentRun) {
        stopCurrentRun(true)
      } else {
        props.exitApp()
      }
      return
    }
    armStop("exit", state.running || currentRun ? "Press Ctrl+C again to stop the agent and quit" : "Press Ctrl+C again to quit Inductor")
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
    const run = currentRun
    if (!run) {
      if (quitAfterStop) props.exitApp()
      setNotice({ text: "No running agent to stop", tone: "muted" })
      setStopArmed(undefined)
      return
    }

    stoppingRun = true
    exitAfterStop = quitAfterStop
    setStopArmed(undefined)
    clearStopArmTimer()
    setNotice({ text: quitAfterStop ? "Stopping agent, then quitting Inductor..." : "Stopping agent...", tone: "cyan" })
    setState(produce((next) => Object.assign(next, markAgentStopped(next))))
    run.interrupt()
    clearForceStopTimer()
    forceStopTimer = setTimeout(() => {
      if (!stoppingRun) return
      run.kill()
      if (exitAfterStop) {
        void run.exited
          .catch(() => undefined)
          .finally(() => props.exitApp())
      }
    }, 5000)
  }

  function clearStopArmTimer() {
    if (!stopArmTimer) return
    clearTimeout(stopArmTimer)
    stopArmTimer = undefined
  }

  function clearForceStopTimer() {
    if (!forceStopTimer) return
    clearTimeout(forceStopTimer)
    forceStopTimer = undefined
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
          title={state.title}
          workspace={props.workspace}
          running={state.running}
          elapsed={formatElapsed(now() - startedAt)}
          openPalette={openPalette}
        />
        <box flexGrow={1} minHeight={0} flexDirection="row" gap={1} paddingLeft={1} paddingRight={1} paddingTop={1} paddingBottom={1}>
          <SessionSidebar
            sessions={sessions()}
            currentSessionId={sessionId()}
            status={sessionListStatus()}
            newSession={startNewSession}
            loadSession={loadSession}
            refreshSessions={refreshSessions}
          />
          <box
            flexGrow={1}
            minWidth={0}
            height="100%"
            flexDirection="column"
            backgroundColor={theme.panel}
            overflow="hidden"
            border
            borderStyle="rounded"
            borderColor={theme.border}
          >
            <Show when={hasTranscript()} fallback={<StartScreen height={dimensions().height} />}>
              <scrollbox
                flexGrow={1}
                minHeight={0}
                overflow="hidden"
                stickyScroll={true}
                stickyStart="bottom"
                scrollAcceleration={scrollAcceleration}
                viewportCulling={true}
                viewportOptions={{ overflow: "hidden" }}
                contentOptions={{ overflow: "hidden" }}
                verticalScrollbarOptions={{ visible: false }}
              >
                <Timeline
                  items={state.transcript}
                  pendingPermission={state.pendingPermission}
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
            state={state}
            provider={provider()}
            model={model()}
            workspace={props.workspace}
            contextPercent={contextPercent()}
            mode={mode()}
            branch="HEAD"
            openModifiedFile={openModifiedFile}
          />
        </box>
        <Composer
          state={state}
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
          activityGlyph={runningGlyph(now())}
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
      <TopMetric width={25} label="effort" value={props.mode} color={theme.cyan} onClick={() => props.openPalette("modes")} />
      <TopMetric width={40} label="agent" value={truncateRight(modelDisplay(props.provider, props.model), 26)} color={theme.blue} onClick={() => props.openPalette("models")} />
      <TopMetric width={40} label="session" value={truncateRight(props.title, 24)} color={theme.cyan} />
      <TopMetric width={25} label="branch" value="HEAD" color={theme.cyan} />
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
      <text fg={theme.dim}>{props.label}</text>
      <text fg={props.color} attributes={TextAttributes.BOLD}>{props.value}</text>
    </box>
  )
}

function SessionSidebar(props: {
  sessions: StoredSession[]
  currentSessionId?: string
  status: string
  newSession: () => void
  loadSession: (id: string) => void
  refreshSessions: () => Promise<void>
}) {
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
      <box
        width="100%"
        flexDirection="row"
        gap={1}
        paddingLeft={1}
        paddingRight={1}
        backgroundColor={!props.currentSessionId ? theme.paletteSelected : theme.panelSoft}
        onMouseUp={props.newSession}
      >
        <text fg={theme.cyan} attributes={TextAttributes.BOLD}>+</text>
        <text fg={theme.text} attributes={TextAttributes.BOLD}>New session</text>
      </box>
      <box flexDirection="row" gap={1} paddingLeft={1} paddingRight={1} marginTop={1} marginBottom={1}>
        <text fg={theme.cyan}>SESSIONS</text>
        <box flexGrow={1} />
      </box>
      <Show when={!props.status} fallback={<text fg={theme.red}>{truncateRight(props.status, SESSION_SIDEBAR_TEXT_WIDTH)}</text>}>
        <scrollbox flexGrow={1} minHeight={0} scrollAcceleration={scrollAcceleration} verticalScrollbarOptions={{ visible: false }}>
          <box flexDirection="column" gap={1}>
            <Show when={props.sessions.length > 0} fallback={<text fg={theme.dim}>No previous sessions</text>}>
              <For each={props.sessions}>
                {(session) => {
                  const active = () => props.currentSessionId === session.id
                  return (
                    <box
                      width="100%"
                      flexDirection="column"
                      paddingLeft={1}
                      paddingRight={1}
                      backgroundColor={active() ? theme.paletteSelected : theme.panelSoft}
                      border={["left"]}
                      borderColor={active() ? theme.cyan : theme.borderSoft}
                      onMouseUp={() => props.loadSession(session.id)}
                    >
                      <text fg={active() ? theme.text : theme.muted} attributes={active() ? TextAttributes.BOLD : undefined} wrapMode="none">
                        {truncateRight(sessionTitle(session), SESSION_SIDEBAR_TEXT_WIDTH)}
                      </text>
                      <box flexDirection="row" gap={1}>
                        <text fg={theme.dim}>{truncateRight(shortModel(session.model), 12)}</text>
                        <text fg={session.status === "running" ? theme.green : theme.dim}>{session.status.toLowerCase()}</text>
                      </box>
                    </box>
                  )
                }}
              </For>
            </Show>
          </box>
        </scrollbox>
      </Show>
    </box>
  )
}

function defaultComposerNotice(status: string, running: boolean, pendingPermission?: AppState["pendingPermission"]): ComposerNotice {
  if (pendingPermission) return { text: "approval required · ↑/↓ choose · enter confirm", tone: "cyan" }
  if (running) return { text: "agent running", tone: "cyan" }
  if (!status || status === "idle") return { text: "ready", tone: "muted" }
  if (status === "stopped") return { text: "stopped agent", tone: "red" }
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
      </box>
    </box>
  )
}

function Timeline(props: {
  items: TranscriptItem[]
  pendingPermission?: AppState["pendingPermission"]
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
        border
        borderStyle="rounded"
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
            border
            borderStyle="rounded"
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
  const viewerHeight = createMemo(() => diffViewerHeight(props.diff))
  return (
    <box width="100%" height={viewerHeight()} minHeight={0} overflow="hidden" flexDirection="column">
      <scrollbox
        width="100%"
        height="100%"
        minHeight={0}
        overflow="hidden"
        scrollX={true}
        scrollY={true}
        stickyScroll={false}
        scrollAcceleration={scrollAcceleration}
        viewportCulling={true}
        viewportOptions={{ overflow: "hidden" }}
        contentOptions={{ overflow: "hidden" }}
        verticalScrollbarOptions={{ visible: false }}
        horizontalScrollbarOptions={{ visible: false }}
      >
        <diff
          diff={props.diff}
          view="split"
          filetype={filetype(props.path)}
          width="100%"
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
      </scrollbox>
    </box>
  )
}

function diffViewerHeight(diff: string) {
  const lineCount = diff.split("\n").length
  return Math.min(DIFF_VIEWER_MAX_HEIGHT, Math.max(DIFF_VIEWER_MIN_HEIGHT, lineCount))
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
        border
        borderStyle="rounded"
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
            <box backgroundColor={theme.panelSoft} border borderStyle="rounded" borderColor={theme.borderStrong} paddingLeft={1} paddingRight={1} paddingTop={1} paddingBottom={1}>
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
  activityGlyph: string
  pasteFromClipboard: () => Promise<void>
}) {
  let textarea!: TextareaRenderable
  const showActivity = () => props.state.running || Boolean(props.state.pendingPermission) || props.notice.tone !== "muted"
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
              {props.state.running ? props.activityGlyph : props.notice.tone === "red" ? "!" : "•"}
            </text>
            <text fg={noticeColor(props.notice)} attributes={props.notice.tone === "cyan" || props.notice.tone === "red" ? TextAttributes.BOLD : undefined}>
              {props.notice.text}
            </text>
          </box>
          <Show when={props.state.running}>
            <text fg={theme.dim}>Esc Esc stop · Ctrl+C Ctrl+C quit</text>
          </Show>
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
            placeholder={props.state.pendingPermission ? "approval required: press 1, 2, or 3" : props.state.running ? "agent is working..." : "Ask INDUCTOR..."}
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
  return provider === "codex" ? "gpt-5.5" : "sonnet"
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
  if (choice?.provider === "codex") {
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

function sessionTitle(session: StoredSession) {
  return session.display_name || summarizeSessionPreview(session.preview) || "New session"
}

function summarizeSessionPreview(value: string) {
  return value.replace(/\s+/g, " ").trim()
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
  if (kind === "write file" || kind === "edit file") {
    return `${toolVerb(item.status, "write")} ${truncateLeft(path ?? description ?? "file", 88)}`
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
  return name === "write_file" || name === "edit_file" || name.toLowerCase().includes("edit")
}

function toolPath(item: Extract<TranscriptItem, { kind: "tool" }>) {
  const json = parseMaybeJson(item.input)
  return stringField(json, ["path", "filepath", "file_path", "target", "filename"])
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
  const direct = stringField(json, ["diff", "patch"])
  const path = toolPath(item) ?? "file"
  if (direct) return normalizeUnifiedPatch(path, direct)
  const oldText = stringField(json, ["old", "old_text", "before"])
  const newText = stringField(json, ["new", "new_text", "content", "after"])
  if (!oldText && !newText) return undefined
  return createUnifiedPatchFromContent(path, oldText ?? "", newText ?? "")
}

function normalizeUnifiedPatch(filePath: string, patch: string) {
  const trimmed = patch.trimStart()
  if (trimmed.startsWith("diff --git ") || trimmed.startsWith("--- ") || trimmed.startsWith("Index: ")) return patch
  if (trimmed.startsWith("@@")) return `--- a/${filePath}\n+++ b/${filePath}\n${patch}`
  return patch
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
