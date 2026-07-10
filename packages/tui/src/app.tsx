/** @jsxImportSource @opentui/solid */
import { stringWidth } from "bun"
import { BoxRenderable, MacOSScrollAccel, SyntaxStyle, TextAttributes, TextareaRenderable, parseColor, type KeyEvent, type OptimizedBuffer } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { execFile } from "node:child_process"
import { promisify } from "node:util"
import path from "node:path"
import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js"
import { createStore, produce } from "solid-js/store"
import {
  applyPermissionDecision,
  applyQuestionAnswers,
  applySessionEvent,
  addUserMessage,
  createInitialState,
  loadStoredSession,
  markAgentStopped,
  type AppState,
  type ModifiedFile,
  type TranscriptItem,
} from "./state"
import { archiveWorktree, listProviderModels, listSkills, listWorktrees, showWorkspaceSession, startBackendTurn, startCopilotLogin, type AuthStatusEvent, type BackendOptions, type BackendRun, type DevMode, type ModelRole, type PermissionDecision, type ProviderModel, type QuestionAnswer, type QuestionItem, type SkillInfo, type Worktree } from "./backend"
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
  type PromptTextAttachment,
} from "./mentions"
import { deletePromptPlaceholderAtCursor, expandPromptPlaceholders, insertTextAtCursor, parsePromptHistory, recordPromptHistory, serializePromptHistory, shouldCompactPastedText, shouldNavigateHistory, stepPromptHistory, type HistoryDirection, type PromptHistoryState, type PromptPlaceholder } from "./prompt_input"
import { spawnTerminalSession, type TerminalSession, type TerminalSnapshot } from "./terminal"
import { isTerminalMouseSequence } from "./terminal_sequences"
export type AppProps = BackendOptions & {
  exitApp(): void
  registerCtrlCHandler(handler: (() => void) | undefined): void
  registerSelectionTransform(transform: ((text: string) => string) | undefined): void
}

type EffortValue = "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultracode"
type ModelRoleConfig = { provider: string; model: string; effort: EffortValue }
type ModelFamily = Record<ModelRole, ModelRoleConfig>

/** One concurrent agent: its session, worktree, provider/model and transcript. */
type AgentSlot = {
  key: string
  sessionId?: string
  workspaceId?: string
  branch: string
  provider: string
  model: string
  effort: EffortValue
  modelFamilyEnabled: boolean
  modelFamily: ModelFamily
  activeModelRole?: ModelRole
  devMode: DevMode
  approval: string
  workspaceOnly: boolean
  role: string
  stateDb?: string
  state: AppState
}

/** Ephemeral per-run bookkeeping for a slot's live subprocess. */
type OrchestrationPhase = "initial" | "executor_report" | "review_feedback"
type OrchestrationDecision = "continue" | "review" | "complete" | undefined
type QueuedPrompt = { visiblePrompt: string; prompt: string; active?: boolean; skills?: string[]; modelRole?: ModelRole; orchestrationRound?: number; orchestrationPhase?: OrchestrationPhase }
type RunFlags = { stopping: boolean; exitAfter: boolean; forceTimer?: ReturnType<typeof setTimeout>; queued: QueuedPrompt[]; activeQueued?: QueuedPrompt; steering?: QueuedPrompt }
type PaletteKind = "commands" | "models" | "model_family" | "connect" | "agents" | "modes" | "permissions" | "skills" | "files" | undefined
type CommandAction = "agents" | "clear" | "connect" | "exit" | "help" | "mode" | "model" | "model_family" | "new" | "permissions" | "pr" | "resume" | "review" | "sessions" | "skills"
type Command = { name: string; description: string; action: CommandAction }
type ModelChoice = { provider: string; model: string; label: string; group: string; effortName: string; efforts: EffortValue[]; effortLabels?: Partial<Record<EffortValue, string>> }
type ModelFamilyChoice = { role: ModelRole; setting: "model" | "effort"; provider?: string; model?: string; effort?: EffortValue; label: string; description: string }
type ModelFamilyWizard = { roleIndex: number; step: "model" | "effort"; family: ModelFamily }
type ConnectChoice = { provider: string; label: string; description: string }
type EffortChoice = { name: string; label: string; description: string; value: EffortValue }
type AgentChoice = { name: string; description: string }
type PermissionChoice = { name: string; label: string; description: string; approval: string; workspaceOnly: boolean }
type SkillChoice = SkillInfo & { label: string }
type PaletteItem = Command | ModelChoice | ModelFamilyChoice | ConnectChoice | EffortChoice | AgentChoice | PermissionChoice | SkillChoice | FileChoice
type SkillMentionState = { triggerStart: number; token: string; query: string }
type SkillCreateFlow = { step: "name" | "description" | "body"; name: string; description: string }
type StopIntent = "interrupt" | "exit"
type PrFlow = { step: "base" | "message"; base: string; worktree: Worktree }
type NoticeTone = "cyan" | "red" | "muted"
type ComposerNotice = { text: string; tone: NoticeTone }
const permissionActions = ["allow", "allow_always", "deny"] as const
const RECOVERED_RESTART_MARKER = "Recovered after Inductor restarted"
const RESUME_PROMPT_MARKER = "__INDUCTOR_RESUME__"

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
  skillOrange: "#ff7a00",
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
const TELEMETRY_FILE_WIDTH = 22
const TELEMETRY_FOOTER_WIDTH = 24
const commands: Command[] = [
  { name: "/agents", description: "Switch agent", action: "agents" },
  { name: "/connect", description: "Connect provider", action: "connect" },
  { name: "/effort", description: "Switch reasoning effort", action: "mode" },
  { name: "/fast", description: "Switch reasoning effort", action: "mode" },
  { name: "/help", description: "Show shortcuts", action: "help" },
  { name: "/model", description: "Switch model", action: "model" },
  { name: "/model_family", description: "Configure reasoning/executor/reviewer models", action: "model_family" },
  { name: "/new", description: "New session", action: "new" },
  { name: "/permissions", description: "Switch agent permissions", action: "permissions" },
  { name: "/pr", description: "Create pull request", action: "pr" },
  { name: "/resume", description: "Resume latest interrupted prompt", action: "resume" },
  { name: "/review", description: "Review changes", action: "review" },
  { name: "/sessions", description: "Open sessions", action: "sessions" },
  { name: "/skill", description: "Create a reusable skill", action: "skills" },
  { name: "/skills", description: "Toggle reusable skills for future prompts", action: "skills" },
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
  { group: "OpenAI", provider: "codex", model: "gpt-5.3-codex", label: "GPT-5.3-Codex", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.3-codex-spark", label: "GPT-5.3-Codex-Spark", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.2", label: "GPT-5.2", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.6-sol", label: "GPT-5.6 Sol", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.6-terra", label: "GPT-5.6 Terra", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
  { group: "OpenAI", provider: "codex", model: "gpt-5.6-luna", label: "GPT-5.6 Luna", effortName: "Reasoning", efforts: ["low", "medium", "high", "xhigh"], effortLabels: { xhigh: "Extra High" } },
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
  { scope: ["inductor.command"], style: { foreground: theme.cyan, bold: true } },
])
const commandHighlightStyle = syntaxStyle.getStyleId("inductor.command")

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

type ModelFamilyPreference = { enabled: boolean; family: ModelFamily }

function loadModelFamilyPreference(filePath: string, fallback: ModelFamily): ModelFamilyPreference {
  try {
    if (!existsSync(filePath)) return { enabled: false, family: cloneModelFamily(fallback) }
    const parsed = JSON.parse(readFileSync(filePath, "utf8")) as Partial<ModelFamilyPreference>
    if (typeof parsed.enabled !== "boolean" || !isModelFamily(parsed.family)) return { enabled: false, family: cloneModelFamily(fallback) }
    return { enabled: parsed.enabled, family: cloneModelFamily(parsed.family) }
  } catch {
    return { enabled: false, family: cloneModelFamily(fallback) }
  }
}

function saveModelFamilyPreference(filePath: string, preference: ModelFamilyPreference) {
  try {
    mkdirSync(path.dirname(filePath), { recursive: true })
    writeFileSync(filePath, JSON.stringify({ enabled: preference.enabled, family: preference.family }, null, 2))
  } catch {
    // model-family persistence is best-effort; ignore write failures
  }
}

function defaultModelFamily(provider: string, model?: string, effort: EffortValue = "medium"): ModelFamily {
  const primaryModel = model ?? defaultModel(provider)
  return {
    reasoning: { provider, model: primaryModel, effort: coerceFamilyEffort("high") },
    executor: { provider, model: defaultExecutorModel(provider), effort: coerceFamilyEffort("low") },
    reviewer: { provider, model: primaryModel, effort: coerceFamilyEffort(effort) },
  }
}

function cloneModelFamily(family: ModelFamily): ModelFamily {
  return {
    reasoning: { ...family.reasoning },
    executor: { ...family.executor },
    reviewer: { ...family.reviewer },
  }
}

function isModelFamily(value: unknown): value is ModelFamily {
  if (!value || typeof value !== "object") return false
  const family = value as Record<ModelRole, Partial<ModelRoleConfig> | undefined>
  return modelFamilyWizardRoles.every((role) => {
    const cfg = family[role]
    return Boolean(cfg && typeof cfg.provider === "string" && typeof cfg.model === "string" && isEffortValue(cfg.effort))
  })
}

function isEffortValue(value: unknown): value is EffortValue {
  return value === "none" || value === "low" || value === "medium" || value === "high" || value === "xhigh" || value === "max" || value === "ultracode"
}

function coerceFamilyEffort(effort: EffortValue): EffortValue {
  return effort === "ultracode" ? "max" : effort
}

function defaultExecutorModel(provider: string) {
  if (provider === "codex") return "gpt-5.4-mini"
  if (provider === "claude") return "haiku"
  if (provider === "copilot") return "o4-mini"
  return defaultModel(provider)
}

const modelFamilyWizardRoles: readonly ModelRole[] = ["reasoning", "executor", "reviewer"]

export function App(props: AppProps) {
  let input!: TextareaRenderable
  let replacingPrompt = false
  let lastCtrlCAt = 0
  let stopArmTimer: ReturnType<typeof setTimeout> | undefined
  const startedAt = Date.now()
  const modelFamilyPreferencePath = path.join(props.workspace, ".inductor", "model-family.json")
  const loadedModelFamilyPreference = loadModelFamilyPreference(modelFamilyPreferencePath, defaultModelFamily(props.provider, props.model, "medium"))
  let defaultModelFamilySelection = cloneModelFamily(loadedModelFamilyPreference.family)
  let defaultModelFamilyEnabled = loadedModelFamilyPreference.enabled

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
      modelFamilyEnabled: init.modelFamilyEnabled ?? defaultModelFamilyEnabled,
      modelFamily: init.modelFamily ? cloneModelFamily(init.modelFamily) : cloneModelFamily(defaultModelFamilySelection),
      activeModelRole: init.activeModelRole,
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
  const autoResumedSessions = new Set<string>()

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
      flags = { stopping: false, exitAfter: false, queued: [] }
      runFlags.set(key, flags)
    }
    return flags
  }

  function queuedPromptsFor(key: string) {
    return runFlags.get(key)?.queued.filter((prompt) => !prompt.active) ?? []
  }

  // Composer settings are views of the focused slot, so existing call sites
  // (provider(), setModel(x), ...) keep working while each agent owns its own.
  const provider = () => focusedAgent().provider
  const setProvider = (value: string) => {
    const current = focusedAgent()
    defaultModelFamilyEnabled = false
    saveModelFamilyPreference(modelFamilyPreferencePath, { enabled: false, family: defaultModelFamilySelection })
    patchFocused({
      provider: value,
      modelFamilyEnabled: false,
      activeModelRole: undefined,
      modelFamily: defaultModelFamily(value, current.model, current.effort),
    })
  }
  const model = () => focusedAgent().model
  const setModel = (value: string) => {
    defaultModelFamilyEnabled = false
    saveModelFamilyPreference(modelFamilyPreferencePath, { enabled: false, family: defaultModelFamilySelection })
    patchFocused({ model: value, modelFamilyEnabled: false, activeModelRole: undefined })
  }
  const mode = () => focusedAgent().effort
  const setMode = (value: EffortValue) => {
    defaultModelFamilyEnabled = false
    saveModelFamilyPreference(modelFamilyPreferencePath, { enabled: false, family: defaultModelFamilySelection })
    patchFocused({ effort: value, modelFamilyEnabled: false, activeModelRole: undefined })
  }
  const devMode = () => focusedAgent().devMode
  const approval = () => focusedAgent().approval
  const setApproval = (value: string) => patchFocused({ approval: value })
  const workspaceOnly = () => focusedAgent().workspaceOnly
  const setWorkspaceOnly = (value: boolean) => patchFocused({ workspaceOnly: value })
  const agent = () => focusedAgent().role
  const setAgent = (value: string) => patchFocused({ role: value })
  const sessionId = () => focusedAgent().sessionId
  const activeModelRole = () => focusedAgent().modelFamilyEnabled ? (focusedAgent().activeModelRole ?? "reasoning") : "reasoning"
  const activeRoleConfig = () => {
    const current = focusedAgent()
    if (!current.modelFamilyEnabled) return { provider: current.provider, model: current.model, effort: current.effort }
    return current.modelFamily[activeModelRole()]
  }
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
  const [questionIndex, setQuestionIndex] = createSignal(0)
  const [questionAnswers, setQuestionAnswers] = createStore<Record<string, string>>({})
  const [questionCustomDrafts, setQuestionCustomDrafts] = createStore<Record<string, string>>({})
  const [questionSelected, setQuestionSelected] = createStore<Record<string, number>>({})
  const [questionWarning, setQuestionWarning] = createSignal("")
  const promptHistoryPath = path.join(props.workspace, ".inductor", "prompt-history.json")
  const [promptHistory, setPromptHistory] = createSignal<PromptHistoryState>({ entries: loadPromptHistoryFile(promptHistoryPath), draft: "" })
  const [palette, setPalette] = createSignal<PaletteKind>()
  const [selected, setSelected] = createSignal(0)
  const [modelFamilyWizard, setModelFamilyWizard] = createSignal<ModelFamilyWizard>()
  const [mention, setMention] = createSignal<MentionState>()
  const [skillMention, setSkillMention] = createSignal<SkillMentionState>()
  const [pasteCount, setPasteCount] = createSignal(0)
  const [promptImages, setPromptImages] = createSignal<PromptImageAttachment[]>([])
  const [promptPastes, setPromptPastes] = createSignal<PromptTextAttachment[]>([])
  const [prFlow, setPrFlow] = createSignal<PrFlow>()
  const [worktrees, setWorktrees] = createSignal<Worktree[]>([])
  const [skills, setSkills] = createSignal<SkillChoice[]>([])
  const [activeSkills, setActiveSkills] = createSignal<string[]>([]) // Explicitly tagged/preloaded; all discovered skills are still advertised to the model.
  const [skillsStatus, setSkillsStatus] = createSignal("")
  const [skillCreateFlow, setSkillCreateFlow] = createSignal<SkillCreateFlow>()
  const [worktreeBusy, setWorktreeBusy] = createSignal<string>()
  const [sessionListStatus, setSessionListStatus] = createSignal("")
  const [expanded, setExpanded] = createSignal<Set<string>>(new Set())
  const [queuedVersion, setQueuedVersion] = createSignal(0)
  const [now, setNow] = createSignal(Date.now())
  const [modelCatalogVersion, setModelCatalogVersion] = createSignal(0)
  const skillHighlightStyle = SyntaxStyle.fromStyles({ skill: { fg: theme.skillOrange, bold: true } })
  const skillHighlightStyleId = skillHighlightStyle.getStyleId("skill") ?? skillHighlightStyle.registerStyle("skill", { fg: theme.skillOrange, bold: true })
  const skillHighlightRef = 4242

  const dimensions = useTerminalDimensions()
  const availableInputWidth = createMemo(() => Math.max(1, dimensions().width - 6))
  const contextPercent = createMemo(() => Math.min(99, Math.round((fstate().tokens / 200_000) * 100)))
  const focusedQueuedPrompts = createMemo(() => {
    queuedVersion()
    return queuedPromptsFor(store.focusedKey)
  })
  const hasTranscript = createMemo(() => fstate().transcript.length > 0 || fstate().running || focusedQueuedPrompts().length > 0 || Boolean(fstate().pendingPermission) || Boolean(fstate().pendingQuestions))
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
  const skillItems = createMemo(() => {
    const active = skillMention()
    if (!active) return skills()
    const query = active.query.toLowerCase()
    const selected = new Set(activeSkills())
    return skills()
      .filter((skill) => !selected.has(skill.name) || skill.name.toLowerCase().includes(query))
      .filter((skill) => skill.name.toLowerCase().includes(query) || skill.description.toLowerCase().includes(query) || skill.source.toLowerCase().includes(query))
  })
  const modelFamilyItems = createMemo<ModelFamilyChoice[]>(() => {
    modelCatalogVersion()
    const wizard = modelFamilyWizard()
    const role = wizard ? modelFamilyWizardRoles[wizard.roleIndex] : undefined
    if (!wizard || !role) return []
    const cfg = wizard.family[role]
    if (wizard.step === "model") {
      return modelChoices.map((choice) => ({
        role,
        setting: "model" as const,
        provider: choice.provider,
        model: choice.model,
        label: choice.label,
        description: `Set ${roleLabel(role)} model`,
      }))
    }
    const choice = selectedModelChoice(cfg.provider, cfg.model)
    return effortChoices(choice).map((effort) => ({
      role,
      setting: "effort" as const,
      effort: effort.value,
      label: effort.label,
      description: `Set ${roleLabel(role)} effort for ${providerLabel(cfg.provider)} ${shortModel(cfg.model)}`,
    }))
  })

  const paletteItems = createMemo(() => {
    if (palette() === "files") return fileItems()
    if (palette() === "models") {
      modelCatalogVersion()
      return modelChoices
    }
    if (palette() === "model_family") {
      return modelFamilyItems()
    }
    if (palette() === "connect") return connectChoices
    if (palette() === "agents") return agentChoices
    if (palette() === "modes") return effortChoices(selectedModelChoice(provider(), model()))
    if (palette() === "permissions") return permissionChoices
    if (palette() === "skills") return skillItems()
    return commandItems()
  })

  const timer = setInterval(() => setNow(Date.now()), 120)
  const composerNotice = createMemo(() => {
    const state = fstate()
    return notice() ?? defaultComposerNotice(state.status, state.running, state.pendingPermission)
  })
  onMount(() => {
    props.registerCtrlCHandler(handleCtrlC)
    props.registerSelectionTransform((text) => expandPromptPlaceholders(text, promptPlaceholders()))
    void refreshWorktrees().then((next) => autoResumeRecoveredWorktrees(next))
    void refreshSkills()
    const worktreeRefreshTimer = setInterval(() => void refreshWorktrees(), 30_000)
    void refreshCodexModels()
    void refreshCopilotModels()
    onCleanup(() => clearInterval(worktreeRefreshTimer))
  })
  createEffect(() => {
    applySkillHighlights(input, draft(), skills(), skillHighlightStyle, skillHighlightStyleId, skillHighlightRef)
  })

  onCleanup(() => {
    props.registerCtrlCHandler(undefined)
    props.registerSelectionTransform(undefined)
    clearInterval(timer)
    clearStopArmTimer()
    for (const flags of runFlags.values()) {
      if (flags.forceTimer) clearTimeout(flags.forceTimer)
    }
    for (const run of runs.values()) run.kill()
    runs.clear()
    runFlags.clear()
    skillHighlightStyle.destroy()
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
    if (isSteerSubmitShortcut(event)) {
      event.preventDefault()
      event.stopPropagation()
      submit(true)
      return
    }
    if (isNewSessionShortcut(event)) {
      event.preventDefault()
      event.stopPropagation()
      startNewSession()
      return
    }
    if (fstate().pendingQuestions && handleQuestionKey(event)) {
      event.preventDefault()
      event.stopPropagation()
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
    if (isEscape(event) && skillCreateFlow()) {
      event.preventDefault()
      event.stopPropagation()
      setSkillCreateFlow(undefined)
      replacePrompt("")
      setNotice({ text: "Skill creation cancelled", tone: "muted" })
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

  function submit(steer = false) {
    const visiblePrompt = input.plainText.trim()
    const prompt = promptForSubmit(visiblePrompt, promptImages(), promptPastes()).trim()
    const promptSkills = extractSkillMentions(visiblePrompt, skills())
    if (palette()) {
      acceptPalette()
      return
    }
    if (prFlow()) {
      void submitPrFlow(visiblePrompt)
      return
    }
    if (skillCreateFlow()) {
      submitSkillCreateFlow(visiblePrompt)
      return
    }
    if (!visiblePrompt) return

    const key = store.focusedKey
    const backendPrompt = resumePromptFromUserPrompt(visiblePrompt, fstate()) ?? prompt
    const queued: QueuedPrompt = {
      visiblePrompt,
      prompt: backendPrompt,
      skills: uniqueStrings([...activeSkills(), ...promptSkills]),
      modelRole: focusedAgent().modelFamilyEnabled ? "reasoning" : undefined,
      orchestrationPhase: focusedAgent().modelFamilyEnabled ? "initial" : undefined,
    }
    const running = fstate().running || runs.has(key)
    if (running) {
      if (steer) replaceCurrentRun(key, queued)
      else queuePrompt(key, queued)
      return
    }

    consumePromptInput(visiblePrompt)
    startTurn(key, queued)
  }

  function consumePromptInput(visiblePrompt: string) {
    input.setText("")
    setDraft("")
    recordHistory(visiblePrompt)
    setPromptImages([])
    setPromptPastes([])
    setPalette(undefined)
    disarmStopWarning()
  }

  function queuePrompt(key: string, prompt: QueuedPrompt) {
    consumePromptInput(prompt.visiblePrompt)
    const flags = runFlagsFor(key)
    flags.queued.push(prompt)
    setQueuedVersion((version) => version + 1)
    setNotice(undefined)
  }

  function replaceCurrentRun(key: string, prompt: QueuedPrompt) {
    const run = runs.get(key)
    if (!run) {
      consumePromptInput(prompt.visiblePrompt)
      startTurn(key, prompt)
      return
    }

    consumePromptInput(prompt.visiblePrompt)
    const flags = runFlagsFor(key)
    flags.stopping = true
    flags.exitAfter = false
    flags.steering = prompt
    flags.activeQueued = undefined
    flags.queued = []
    setQueuedVersion((version) => version + 1)
    setStopArmed(undefined)
    clearStopArmTimer()
    setNotice({ text: "Stopping current agent, then starting new prompt...", tone: "cyan" })
    updateAgentState(key, (next) => markAgentStopped(next))
    run.interrupt()
    if (flags.forceTimer) clearTimeout(flags.forceTimer)
    flags.forceTimer = setTimeout(() => {
      if (!flags.stopping) return
      run.kill()
    }, 5000)
  }

  function startTurn(key: string, queued: QueuedPrompt) {
    const agentSlot = store.agents[agentIndex(key)]
    if (!agentSlot) return

    const flags = runFlagsFor(key)
    const wasQueued = flags.activeQueued === queued || flags.queued.includes(queued)
    flags.stopping = false
    flags.exitAfter = false
    if (wasQueued) {
      queued.active = true
      flags.activeQueued = queued
      setQueuedVersion((version) => version + 1)
    }
    if (flags.forceTimer) {
      clearTimeout(flags.forceTimer)
      flags.forceTimer = undefined
    }

    setNotice(undefined)
    updateAgentState(key, (next) => addUserMessage(next, queued.visiblePrompt))
    const runRole = agentSlot.modelFamilyEnabled ? (queued.modelRole ?? "reasoning") : "reasoning"
    const runConfig = agentSlot.modelFamilyEnabled
      ? agentSlot.modelFamily[runRole]
      : { provider: agentSlot.provider, model: agentSlot.model, effort: agentSlot.effort }
    patchAgent(key, { activeModelRole: agentSlot.modelFamilyEnabled ? runRole : undefined })
    const run = startBackendTurn(queued.prompt, {
      ...props,
      provider: runConfig.provider,
      model: runConfig.model,
      approval: agentSlot.approval,
      workspaceOnly: agentSlot.workspaceOnly,
      sessionId: agentSlot.sessionId,
      effort: backendEffort(runConfig.effort),
      modelRole: agentSlot.modelFamilyEnabled ? runRole : undefined,
      mode: agentSlot.devMode,
      appDb: props.appDb,
      // Reuse the worktree once the session owns one; the backend creates it on
      // the first prompt (named after the work) when these are absent.
      workspaceId: agentSlot.workspaceId,
      stateDb: agentSlot.stateDb,
      skills: queued.skills ?? activeSkills(),
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
        if (event.type === "questions_requested" && store.focusedKey === key) resetQuestionUi(event.questions)
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
        if (flags.activeQueued === queued) {
          flags.activeQueued = undefined
          flags.queued = flags.queued.filter((prompt) => prompt !== queued)
          setQueuedVersion((version) => version + 1)
        }
        if (flags.exitAfter) {
          props.exitApp()
          return
        }
        if (flags.stopping) {
          const steering = flags.steering
          flags.stopping = false
          flags.steering = undefined
          updateAgentState(key, (next) => markAgentStopped(next))
          void refreshWorktrees()
          if (steering) {
            setNotice({ text: "Starting new prompt...", tone: "cyan" })
            startTurn(key, steering)
          } else {
            setNotice({ text: "stopped agent", tone: "red" })
          }
          return
        }
        void refreshWorktrees()
        const followup = orchestrationFollowup(key, queued)
        if (followup) {
          setNotice({ text: `Starting ${roleLabel(followup.modelRole ?? "reasoning").toLowerCase()} role...`, tone: "cyan" })
          startTurn(key, followup)
          return
        }
        patchAgent(key, { activeModelRole: undefined })
        updateAgentState(key, (next) => ({ ...next, running: false, status: code === 0 ? "idle" : `exited ${code ?? "unknown"}` }))
        const nextPrompt = flags.queued.find((prompt) => !prompt.active)
        if (nextPrompt) {
          setNotice(undefined)
          startTurn(key, nextPrompt)
        }
      },
    })
    runs.set(key, run)
  }

  function orchestrationFollowup(key: string, queued: QueuedPrompt): QueuedPrompt | undefined {
    const slot = store.agents[agentIndex(key)]
    if (!slot?.modelFamilyEnabled) return undefined
    const round = queued.orchestrationRound ?? 0
    if (queued.modelRole === "reasoning") {
      const decision = latestOrchestrationDecision(fstate().transcript)
      if (decision === "complete") {
        return undefined
      }
      if (decision === "review") {
        return {
          visiblePrompt: "reviewer: independently review executor work",
          prompt: "You are now the reviewer role. Independently review the model-family work from this session for correctness, regressions, safety, and missed requirements. Do not rely on the executor's final summary. Read the full transcript context: the original user request, all reasoning instructions, executor tool-call requests and tool results, especially apply_patch/edit/write operations, patch diagnostics, and verification commands/results. Do not edit files or call tools. Return concrete findings only, then clearly recommend whether reasoning should accept the work as complete or continue with another executor cycle.",
          skills: queued.skills,
          modelRole: "reviewer",
          orchestrationRound: round,
          orchestrationPhase: "review_feedback",
        }
      }
      if (decision && decision !== "continue") return undefined
      if (!decision && queued.orchestrationPhase === "review_feedback") return undefined
      return {
        visiblePrompt: round === 0 ? "executor: execute reasoning instructions" : "executor: continue reasoning instructions",
        prompt: "You are now the executor role. Execute only the latest reasoning model instructions from the transcript. Use tools to inspect, edit, and verify as requested. Do not decide the final design, broaden scope, or treat your own result as complete. When the requested executor slice is done or blocked, report a concise EXECUTOR_REPORT with: tools run, facts discovered, files changed, verification results, blockers, and any uncertainty. Then stop so the reasoning model can decide the next executor slice, review, or completion.",
        skills: queued.skills,
        modelRole: "executor",
        orchestrationRound: round,
        orchestrationPhase: "executor_report",
      }
    }
    if (queued.modelRole === "executor") {
      return {
        visiblePrompt: "reasoning: evaluate executor report",
        prompt: "You are now the reasoning role. Evaluate the executor's latest report, tool-call transcript, tool results, and any file changes from this executor slice. You own the final diff and must decide the next model-family step. If more inspection, editing, or verification is needed, give the next exact executor instructions and end with `ORCHESTRATION_DECISION: continue`. If the implementation diff is ready for independent review, summarize what should be reviewed and end with `ORCHESTRATION_DECISION: review`. Do not end with `ORCHESTRATION_DECISION: complete` until after reviewer feedback has been evaluated.",
        skills: queued.skills,
        modelRole: "reasoning",
        orchestrationRound: round,
        orchestrationPhase: "executor_report",
      }
    }
    if (queued.modelRole === "reviewer") {
      return {
        visiblePrompt: "reasoning: evaluate reviewer feedback",
        prompt: "You are now the reasoning role. Evaluate the reviewer feedback against the original user request, your prior reasoning instructions, and the executor's tool-call transcript/diffs. You own the final diff decision. If more work is needed, produce the next exact executor instructions and end with `ORCHESTRATION_DECISION: continue`. If another independent review is needed without executor work, end with `ORCHESTRATION_DECISION: review`. Only when you are satisfied that the implementation and review show the task is fully complete should you give the final user-facing summary and end with `ORCHESTRATION_DECISION: complete`.",
        skills: queued.skills,
        modelRole: "reasoning",
        orchestrationRound: round + 1,
        orchestrationPhase: "review_feedback",
      }
    }
    return undefined
  }

  function latestOrchestrationDecision(transcript: TranscriptItem[]): OrchestrationDecision {
    const latest = latestAssistantText(transcript).toLowerCase()
    if (!latest) return undefined
    if (/\borchestration_decision\s*:\s*complete\b/.test(latest)) return "complete"
    if (/\borchestration_decision\s*:\s*review\b/.test(latest)) return "review"
    if (/\borchestration_decision\s*:\s*continue\b/.test(latest)) return "continue"
    if (/\btask_complete\s*:\s*true\b/.test(latest)) return "complete"
    if (/\btask_complete\s*:\s*false\b/.test(latest)) return "continue"
    // Backstop for older prompts or models that did not emit the explicit
    // control line: if the reasoning pass says the review is valid and gives
    // next executor actions, keep the loop moving instead of stopping.
    if (
      latest.includes("next executor action") ||
      latest.includes("next executor step") ||
      latest.includes("next exact executor") ||
      (latest.includes("reviewer feedback is valid") && latest.includes("incomplete"))
    ) {
      return "continue"
    }
    if (latest.includes("ready for review") || latest.includes("independent review")) return "review"
    return undefined
  }

  function latestAssistantText(transcript: TranscriptItem[]) {
    for (let index = transcript.length - 1; index >= 0; index -= 1) {
      const item = transcript[index]
      if (item.kind === "assistant") return item.text.trim()
    }
    return ""
  }

  function resumePromptFromUserPrompt(prompt: string, _state: AppState) {
    if (!isResumePrompt(prompt)) return undefined
    return resumeBackendPrompt(prompt)
  }

  function isResumePrompt(prompt: string) {
    return /^(?:\/|(?:inductor\s+)?)resume(?:\s+(?:please|this|it|that|thing|thingy))?[\s.!?]*$/i.test(prompt.trim())
  }

  function resumeBackendPrompt(visiblePrompt: string) {
    return `${RESUME_PROMPT_MARKER}:${JSON.stringify({ visible_prompt: visiblePrompt })}`
  }

  function autoResumeInstruction(latestPrompt: string) {
    return resumeBackendPrompt(latestPrompt)
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
      setSkillMention(undefined)
      openPalette("files")
      return
    }
    const activeSkillMention = findActiveSkillMention(value)
    if (activeSkillMention) {
      setSkillMention(activeSkillMention)
      setMention(undefined)
      void refreshSkills()
      openPalette("skills")
      return
    }
    if (palette() === "files") {
      setMention(undefined)
      setPalette(undefined)
      setSelected(0)
    }
    if (palette() === "skills" && skillMention()) {
      setSkillMention(undefined)
      setPalette(undefined)
      setSelected(0)
    }
    if (value.startsWith("/") && !/\s/.test(value)) {
      const hasMatches = commands.some((command) => command.name.startsWith(value))
      if (hasMatches) openPalette("commands")
      else if (palette() === "commands") {
        setPalette(undefined)
        setSelected(0)
      }
    } else if (palette() === "commands") {
      setPalette(undefined)
      setSelected(0)
    }
    void normalizeImagePathPaste(value)
  }

  function promptPlaceholders(): PromptPlaceholder[] {
    return [
      ...promptImages().map((image) => ({ label: image.label, replacement: `${image.label} @${image.path}` })),
      ...promptPastes().map((paste) => ({ label: paste.label, replacement: paste.text })),
      ...skillPlaceholders(input.plainText, skills()),
    ]
  }
  function deletePromptPlaceholder(direction: "backward" | "forward") {
    const next = deletePromptPlaceholderAtCursor(input.plainText, input.cursorOffset, promptPlaceholders(), direction)
    if (!next.deleted) return false
    input.setText(next.value)
    input.cursorOffset = next.cursorOffset
    updateDraft(next.value)
    return true
  }

  function dismissPalette() {
    if (palette() === "model_family") setModelFamilyWizard(undefined)
    setPalette(undefined)
    setMention(undefined)
    setSkillMention(undefined)
    setSelected(0)
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
    if (kind === "model_family") {
      const current = focusedAgent()
      setModelFamilyWizard({ roleIndex: 0, step: "model", family: cloneModelFamily(current.modelFamilyEnabled ? current.modelFamily : defaultModelFamilySelection) })
      setNotice({ text: `${roleLabel(modelFamilyWizardRoles[0])} model`, tone: "cyan" })
    } else {
      setModelFamilyWizard(undefined)
    }
    setPalette(kind)
    setSelected(0)
  }

  function moveSelection(delta: number) {
    const count = paletteItems().length
    if (count === 0) return
    setSelected((index) => (index + delta + count) % count)
  }

  function acceptModelFamilyChoice(choice: ModelFamilyChoice) {
    const wizard = modelFamilyWizard()
    const role = wizard ? modelFamilyWizardRoles[wizard.roleIndex] : undefined
    if (!wizard || !role || choice.role !== role) return
    const nextFamily = cloneModelFamily(wizard.family)

    if (wizard.step === "model" && choice.setting === "model" && choice.provider && choice.model) {
      const modelChoice = selectedModelChoice(choice.provider, choice.model)
      nextFamily[role] = {
        ...nextFamily[role],
        provider: choice.provider,
        model: choice.model,
        effort: modelChoice ? coerceEffortForModel(nextFamily[role].effort, modelChoice) : nextFamily[role].effort,
      }
      setModelFamilyWizard({ roleIndex: wizard.roleIndex, step: "effort", family: nextFamily })
      setNotice({ text: `${roleLabel(role)} effort`, tone: "cyan" })
      setSelected(0)
      return
    }

    if (wizard.step === "effort" && choice.setting === "effort" && choice.effort) {
      nextFamily[role] = { ...nextFamily[role], effort: choice.effort }
      const nextRoleIndex = wizard.roleIndex + 1
      const nextRole = modelFamilyWizardRoles[nextRoleIndex]
      if (!nextRole) {
        defaultModelFamilySelection = cloneModelFamily(nextFamily)
        defaultModelFamilyEnabled = true
        saveModelFamilyPreference(modelFamilyPreferencePath, { enabled: true, family: defaultModelFamilySelection })
        patchFocused({
          modelFamilyEnabled: true,
          modelFamily: nextFamily,
          activeModelRole: undefined,
          provider: nextFamily.reasoning.provider,
          model: nextFamily.reasoning.model,
          effort: nextFamily.reasoning.effort,
        })
        setNotice({ text: "model family configured", tone: "muted" })
        setModelFamilyWizard(undefined)
        closePalette()
        return
      }
      setModelFamilyWizard({ roleIndex: nextRoleIndex, step: "model", family: nextFamily })
      setNotice({ text: `${roleLabel(nextRole)} model`, tone: "cyan" })
      setSelected(0)
      return
    }
  }

  function acceptSingleModelChoice(choice: ModelChoice) {
    const nextEffort = coerceEffortForModel(mode(), choice)
    defaultModelFamilyEnabled = false
    saveModelFamilyPreference(modelFamilyPreferencePath, { enabled: false, family: defaultModelFamilySelection })
    patchFocused({
      provider: choice.provider,
      model: choice.model,
      effort: nextEffort,
      modelFamilyEnabled: false,
      activeModelRole: undefined,
      modelFamily: defaultModelFamily(choice.provider, choice.model, nextEffort),
    })
    setNotice({ text: `model family off · using ${choice.label}`, tone: "muted" })
    closePalette()
  }

  function acceptPalette(insertDirectory = false) {
    const item = paletteItems()[selected()]
    if (!item) return
    if (palette() === "files") {
      acceptFileChoice(item as FileChoice, insertDirectory)
      return
    }
    if (palette() === "models") {
      acceptSingleModelChoice(item as ModelChoice)
      return
    }
    if (palette() === "model_family") {
      acceptModelFamilyChoice(item as ModelFamilyChoice)
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
    if (palette() === "skills") {
      acceptSkillChoice(item as SkillChoice)
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
      acceptSingleModelChoice(item as ModelChoice)
      return
    }
    if (palette() === "model_family") {
      acceptModelFamilyChoice(item as ModelFamilyChoice)
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
    if (palette() === "skills") {
      acceptSkillChoice(item as SkillChoice)
      return
    }
    runCommand(item as Command)
  }

  function toggleSkill(choice: SkillChoice) {
    const isEnabled = activeSkills().includes(choice.name)
    setActiveSkills((current) => isEnabled ? current.filter((name) => name !== choice.name) : [...current, choice.name])
    setNotice({ text: `${isEnabled ? "unpreloaded" : "preloaded"} skill ${choice.name} · all skills are always visible to the model`, tone: "muted" })
    closePalette()
  }

  function acceptSkillChoice(choice: SkillChoice) {
    const inline = skillMention()
    if (!inline) {
      toggleSkill(choice)
      return
    }

    const next = replaceSkillMention(input.plainText, inline, choice)
    input.setText(next)
    input.cursorOffset = inline.triggerStart + choice.name.length + 2
    setDraft(next)
    setNotice({ text: `tagged skill ${choice.name} · model can also invoke any listed skill`, tone: "muted" })
    closePalette(true)
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

  function closePalette(keepPrompt = false) {
    if (palette() === "model_family") setModelFamilyWizard(undefined)
    setPalette(undefined)
    setMention(undefined)
    setSkillMention(undefined)
    setSelected(0)
    if (!keepPrompt) {
      input.setText("")
      setDraft("")
    }
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

  async function refreshCodexModels() {
    try {
      const models = await listProviderModels(props, "codex")
      if (models.length === 0) return
      const next = [
        ...modelChoices.filter((choice) => choice.provider !== "codex"),
        ...models.map((model) => ({
          group: "OpenAI",
          provider: "codex",
          model: model.id,
          label: model.display_name || model.id,
          effortName: "Reasoning",
          efforts: ["low", "medium", "high", "xhigh"] as EffortValue[],
          effortLabels: { xhigh: "Extra High" },
        })),
      ]
      modelChoices = next
      setModelCatalogVersion((version) => version + 1)
    } catch {
      // Keep the baked-in Codex choices when auth is absent or stale.
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
    const compact = shouldCompactPastedText(text)
    const insert = compact ? addPastedText(text) : text
    const next = insertTextAtCursor(input.plainText, insert, input.cursorOffset)
    input.setText(next.value)
    input.cursorOffset = next.cursorOffset
    updateDraft(next.value)
  }

  function addPastedText(text: string) {
    const nextIndex = promptPastes().length + 1
    const label = `[Pasted text #${nextIndex}]`
    setPromptPastes((current) => [...current, { label, text }])
    return label
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
    if (command.action === "model_family") {
      openPalette("model_family")
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
    if (command.action === "skills") {
      if (command.name === "/skill") {
        closePalette()
        setSkillCreateFlow({ step: "name", name: "", description: "" })
        replacePrompt("")
        setNotice({ text: "Skill name · enter to continue, or use $skill in a prompt to activate existing skills", tone: "cyan" })
        return
      }
      closePalette(true)
      void refreshSkills()
      openPalette("skills")
      setNotice({ text: "Preload/tag skills; every discovered skill is listed to the model by default", tone: "cyan" })
      return
    }
    if (command.action === "pr") {
      closePalette()
      startPullRequestFlow()
      return
    }
    if (command.action === "resume") {
      closePalette()
      input.setText("/resume")
      setDraft("/resume")
      submit()
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

  function submitSkillCreateFlow(value: string) {
    const flow = skillCreateFlow()
    if (!flow) return
    if (flow.step === "name") {
      const name = value.trim()
      if (!name) {
        setNotice({ text: "Skill name is required", tone: "red" })
        return
      }
      setSkillCreateFlow({ step: "description", name, description: "" })
      replacePrompt("")
      setNotice({ text: `Description for ${name} · when should agents use this skill?`, tone: "cyan" })
      return
    }
    if (flow.step === "description") {
      const description = value.trim()
      if (!description) {
        setNotice({ text: "Skill description is required", tone: "red" })
        return
      }
      setSkillCreateFlow({ ...flow, step: "body", description })
      replacePrompt("Describe the exact steps, constraints, examples, and files/tools this skill should use.")
      setNotice({ text: `Instructions for ${flow.name} · edit or press enter for the template`, tone: "cyan" })
      return
    }

    const body = value.trim() || "Describe the exact steps, constraints, examples, and files/tools this skill should use."
    const skillPath = createWorkspaceSkill(props.workspace, flow.name, flow.description, body)
    input.setText("")
    setDraft("")
    setSkillCreateFlow(undefined)
    recordHistory(`/skill ${flow.name}`)
    setNotice({ text: `created skill ${sanitizeSkillName(flow.name)} at ${toWorkspacePath(skillPath)}`, tone: "muted" })
    void refreshSkills()
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
      const { stdout: ahead } = await execFileAsync("git", ["-C", worktree.worktree_path, "rev-list", "--count", `origin/${base}..HEAD`], { maxBuffer: 1024 * 1024 })
      if (ahead.trim() === "0") {
        throw new Error(`no changes to commit; current branch ${worktree.branch_name} has no commits ahead of origin/${base}`)
      }
      await execFileAsync("git", ["-C", worktree.worktree_path, "push", "-u", "origin", worktree.branch_name], { maxBuffer: 1024 * 1024 * 4 })
      let url = ""
      try {
        const { stdout } = await execFileAsync("gh", ["pr", "create", "--head", worktree.branch_name, "--base", base, "--title", message, "--body", "Created by Inductor."], ghExecOptions(worktree.worktree_path))
        url = findUrl(stdout)
      } catch (error) {
        const existing = await existingPullRequestUrl(worktree)
        if (!existing) throw error
        url = existing
      }
      if (!url) url = await existingPullRequestUrl(worktree)
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
    const slot = makeAgentSlot({ provider: base.provider, model: base.model, effort: base.effort, modelFamilyEnabled: base.modelFamilyEnabled, modelFamily: base.modelFamily, devMode: base.devMode, approval: base.approval, workspaceOnly: base.workspaceOnly, role: base.role })
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
      const fresh = makeAgentSlot({ provider: base.provider, model: base.model, effort: base.effort, modelFamilyEnabled: base.modelFamilyEnabled, modelFamily: base.modelFamily, devMode: base.devMode, approval: base.approval, workspaceOnly: base.workspaceOnly, role: base.role })
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
      const visible = next.filter((w) => w.status !== "archived")
      setWorktrees(visible)
      setSessionListStatus("")
      return visible
    } catch (error) {
      setSessionListStatus(error instanceof Error ? error.message : "Could not load worktrees")
      return []
    }
  }

  async function refreshSkills() {
    setSkillsStatus("loading")
    try {
      const next = await listSkills(props)
      setSkills(next.map((skill) => ({ ...skill, label: skill.name })))
      setSkillsStatus("")
    } catch (error) {
      setSkills([])
      setSkillsStatus(error instanceof Error ? error.message : "Could not load skills")
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

  async function autoResumeRecoveredWorktrees(visibleWorktrees: Worktree[]) {
    for (const worktree of visibleWorktrees) {
      if (!worktree.session_id || !worktree.state_db) continue
      if (autoResumedSessions.has(worktree.session_id)) continue
      const open = agentForWorktree(worktree)
      if (open?.state.running || runs.has(open?.key ?? "")) continue

      let detail
      try {
        detail = await showWorkspaceSession(props, worktree.session_id, worktree.state_db)
      } catch {
        continue
      }
      if (!shouldAutoResumeRecoveredSession(detail)) continue

      const latestPrompt = latestUserPromptFromSession(detail)
      if (!latestPrompt) continue
      autoResumedSessions.add(worktree.session_id)

      const key = open?.key ?? openRecoveredWorktreeSlot(worktree, detail)
      if (!key) continue
      setStore("focusedKey", key)
      setExpanded(new Set<string>())
      queueMicrotask(() => {
        setNotice({ text: "Resuming interrupted prompt after restart...", tone: "cyan" })
        startTurn(key, { visiblePrompt: latestPrompt, prompt: autoResumeInstruction(latestPrompt), skills: activeSkills() })
      })
    }
  }

  function openRecoveredWorktreeSlot(worktree: Worktree, detail: Awaited<ReturnType<typeof showWorkspaceSession>>) {
    const slot = makeAgentSlot({
      sessionId: worktree.session_id ?? undefined,
      workspaceId: worktree.workspace_id,
      branch: worktree.branch_name,
      provider: sessionProvider(detail.session.provider_id) ?? props.provider,
      model: detail.session.model || (props.model ?? defaultModel(props.provider)),
      devMode: "worktree",
      stateDb: worktree.state_db ?? undefined,
      state: loadStoredSession(detail),
    })
    setStore("agents", (agents) => [...agents, slot])
    return slot.key
  }

  function shouldAutoResumeRecoveredSession(detail: Awaited<ReturnType<typeof showWorkspaceSession>>) {
    const events = detail.events ?? []
    for (let index = events.length - 1; index >= 0; index -= 1) {
      const event = events[index]
      if (event.type === "status") continue
      return event.type === "error" && typeof event.message === "string" && event.message.includes(RECOVERED_RESTART_MARKER)
    }
    return false
  }

  function latestUserPromptFromSession(detail: Awaited<ReturnType<typeof showWorkspaceSession>>) {
    const events = detail.events ?? []
    for (let index = events.length - 1; index >= 0; index -= 1) {
      const event = events[index]
      if (event.type !== "user_message") continue
      const text = typeof event.text === "string" ? event.text.trim() : ""
      if (text && !isResumePrompt(text)) return text
    }
    for (let index = detail.messages.length - 1; index >= 0; index -= 1) {
      const message = detail.messages[index]
      if (message.role.toLowerCase() !== "user") continue
      const text = message.content.trim()
      if (text && !isResumePrompt(text)) return text
    }
    return undefined
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

  function resetQuestionUi(questions: QuestionItem[] | undefined) {
    setQuestionIndex(0)
    setQuestionWarning("")
    for (const key of Object.keys(questionAnswers)) setQuestionAnswers(key, undefined as unknown as string)
    for (const key of Object.keys(questionCustomDrafts)) setQuestionCustomDrafts(key, undefined as unknown as string)
    for (const key of Object.keys(questionSelected)) setQuestionSelected(key, undefined as unknown as number)
    ;(questions ?? []).forEach((question, index) => {
      const recommended = question.recommended
      const recommendedIndex = recommended ? (question.options ?? []).findIndex((option) => option.label === recommended) : -1
      if (recommendedIndex >= 0) {
        setQuestionSelected(String(index), recommendedIndex)
        setQuestionAnswers(String(index), recommended ?? "")
      }
    })
  }

  function handleQuestionKey(event: KeyEvent) {
    const pending = fstate().pendingQuestions
    if (!pending || pending.questions.length === 0) return false
    const key = permissionKey(event)
    const current = pending.questions[questionIndex()]
    const options = current?.options ?? []
    const customIndex = options.length
    const optionCount = options.length + 1
    const selectedForCurrent = () => Math.min(questionSelected[String(questionIndex())] ?? 0, optionCount - 1)
    if (key === "arrowright" || key === "right" || key === "tab") {
      setQuestionIndex((index) => Math.min(pending.questions.length - 1, index + 1))
      setQuestionWarning("")
      return true
    }
    if (key === "arrowleft" || key === "left" || key === "shifttab") {
      setQuestionIndex((index) => Math.max(0, index - 1))
      setQuestionWarning("")
      return true
    }
    if (key === "arrowup" || key === "up" || key === "k") {
      setQuestionSelected(String(questionIndex()), (selectedForCurrent() + optionCount - 1) % optionCount)
      setQuestionWarning("")
      return true
    }
    if (key === "arrowdown" || key === "down" || key === "j") {
      setQuestionSelected(String(questionIndex()), (selectedForCurrent() + 1) % optionCount)
      setQuestionWarning("")
      return true
    }
    const typed = questionPrintableKey(event, key)
    if (typed) {
      const selected = selectedForCurrent()
      if (selected === customIndex) {
        setQuestionCustomDrafts(String(questionIndex()), `${questionCustomDrafts[String(questionIndex())] ?? ""}${typed}`)
        setQuestionWarning("")
      }
      return true
    }
    if (key === "backspace" || key === "delete") {
      const selected = selectedForCurrent()
      if (selected === customIndex) {
        const currentDraft = questionCustomDrafts[String(questionIndex())] ?? ""
        setQuestionCustomDrafts(String(questionIndex()), key === "delete" ? "" : currentDraft.slice(0, -1))
        setQuestionWarning("")
        return true
      }
    }
    if (/^[1-9]$/.test(key)) {
      const selected = Number(key) - 1
      if (selected < optionCount) {
        setQuestionSelected(String(questionIndex()), selected)
        if (selected < options.length) setQuestionAnswers(String(questionIndex()), options[selected]?.label ?? "")
        setQuestionWarning("")
      }
      return true
    }
    if (key === "enter" || key === "return" || key === "\r" || key === "\n") {
      const index = questionIndex()
      const selected = selectedForCurrent()
      const customAnswer = (questionCustomDrafts[String(index)] ?? "").trim()
      if (selected === customIndex) {
        if (!customAnswer) {
          setQuestionWarning("Write a custom answer in the box below Custom answer, or choose an option")
          return true
        }
        setQuestionAnswers(String(index), customAnswer)
      } else {
        const optionAnswer = options[selected]?.label ?? ""
        if (optionAnswer) setQuestionAnswers(String(index), optionAnswer)
      }
      if (index < pending.questions.length - 1) {
        setQuestionIndex((currentIndex) => currentIndex + 1)
        setQuestionWarning("")
      } else {
        submitQuestions()
      }
      return true
    }
    return false
  }

  function submitQuestions() {
    const pending = fstate().pendingQuestions
    const run = runs.get(store.focusedKey)
    if (!pending || !run) return
    const focusedIndex = questionIndex()
    const focusedOptions = pending.questions[focusedIndex]?.options ?? []
    if ((questionSelected[String(focusedIndex)] ?? 0) === focusedOptions.length) {
      const custom = (questionCustomDrafts[String(focusedIndex)] ?? "").trim()
      if (custom) setQuestionAnswers(String(focusedIndex), custom)
    }
    const answers = pending.questions.map((question, index) => {
      const options = question.options ?? []
      const selected = Math.min(questionSelected[String(index)] ?? 0, options.length)
      const optionAnswer = selected < options.length ? options[selected]?.label ?? "" : ""
      const customAnswer = selected === options.length ? (questionCustomDrafts[String(index)] ?? "").trim() : ""
      return {
        question: question.question ?? `Question ${index + 1}`,
        answer: (questionAnswers[String(index)] || customAnswer || optionAnswer).trim(),
      }
    })
    const missing = answers.findIndex((answer) => !answer.answer)
    if (missing >= 0) {
      setQuestionIndex(missing)
      setQuestionWarning("Answer all questions before submitting")
      return
    }
    run.respondQuestions(pending.toolCallId, answers)
    updateAgentState(store.focusedKey, (next) => applyQuestionAnswers(next, answers))
    resetQuestionUi(undefined)
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
          modelMode={focusedAgent().modelFamilyEnabled ? "model-family" : "model"}
          mode={activeRoleConfig().effort}
          agent={focusedAgent().modelFamilyEnabled ? activeModelRole() : agent()}
          provider={activeRoleConfig().provider}
          model={activeRoleConfig().model}
          title={fstate().title}
          workspace={props.workspace}
          running={fstate().running}
          elapsed={formatElapsed(now() - startedAt)}
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
                  items={fstate().transcript}
                  queuedPrompts={focusedQueuedPrompts()}
                  pendingPermission={fstate().pendingPermission}
                  pendingQuestions={fstate().pendingQuestions}
                  running={fstate().running}
                  runningStatus={fstate().status}
                  activityGlyph={runningGlyph(now())}
                  permissionSelected={permissionSelected()}
                  selectPermission={setPermissionSelected}
                  questionIndex={questionIndex()}
                  questionAnswers={questionAnswers}
                  questionCustomDrafts={questionCustomDrafts}
                  questionSelected={questionSelected}
                  questionWarning={questionWarning()}
                  setQuestionAnswer={(index, value) => setQuestionAnswers(String(index), value)}
                  setQuestionCustomDraft={(index, value) => setQuestionCustomDrafts(String(index), value)}
                  setQuestionSelected={(index, value) => setQuestionSelected(String(index), value)}
                  setQuestionIndex={setQuestionIndex}
                  submitQuestions={submitQuestions}
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
          activeSkills={activeSkills()}
          inputRef={(ref) => (input = ref)}
          draft={draft}
          inputWidth={availableInputWidth()}
          setDraft={updateDraft}
          submit={submit}
          palette={palette}
          paletteItems={paletteItems}
          skillsStatus={skillsStatus()}
          selected={selected}
          moveSelection={moveSelection}
          dismissPalette={dismissPalette}
          acceptPalette={acceptPalette}
          choosePalette={choosePalette}
          openPalette={openPalette}
            insertPromptNewline={insertPromptNewline}
            navigatePromptHistory={navigatePromptHistory}
            notice={composerNotice()}
            pasteFromClipboard={pasteFromClipboard}
            insertPromptText={insertPromptText}
            deletePromptPlaceholder={deletePromptPlaceholder}
          />
      </box>
    </box>
  )
}

function TopRail(props: {
  modelMode: "model" | "model-family"
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
      <TopMetric width={30} label="mode" value={props.modelMode} color={theme.cyan} onClick={() => props.openPalette(props.modelMode === "model-family" ? "model_family" : "models")} />
      <TopMetric width={22} label="effort" value={props.mode} color={theme.cyan} onClick={() => props.openPalette(props.modelMode === "model-family" ? "model_family" : "modes")} />
      <TopMetric width={32} label="agent" value={truncateRight(`${modelDisplay(props.provider, props.model)} (${props.agent})`, 20)} color={theme.blue} onClick={() => props.openPalette(props.modelMode === "model-family" ? "model_family" : "models")} />
      <TopMetric width={36} label="session" value={truncateRight(props.title, 22)} color={theme.cyan} />
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
  // Draw one block cursor at the PTY cursor position while this panel has
  // focus. OpenTUI's native cursor belongs to the composer and is hidden below.
  const [terminalFocused, setTerminalFocused] = createSignal(false)
  const cursorVisible = () => terminalFocused()
  // vt100 screen contents render as a fixed grid; strip trailing blank rows so
  // the prompt hugs the bottom instead of padding out the panel.
  const snapshotLines = () => {
    const snapshot = props.snapshot()
    if (!snapshot) return { rows: [] as string[], cursorRow: -1, cursorCol: 0 }
    const grid = snapshot.screen_rows ?? snapshot.contents.split("\n")
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
    if (isTerminalMouseSequence(data)) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
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
        ref.on("focused", () => {
          setTerminalFocused(true)
          // The composer owns OpenTUI's native cursor. Hide its last position
          // while this custom terminal surface is focused so only the PTY
          // cursor at the active prompt is visible.
          ref.ctx.setCursorPosition(0, 0, false)
        })
        ref.on("blurred", () => setTerminalFocused(false))
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
  return (
    <Show
      when={props.cursorCol >= 0}
      fallback={<text fg={theme.muted} wrapMode="none" selectable={true}>{props.text.length ? props.text : " "}</text>}
    >
      <TerminalCursorLine text={props.text} cursorCol={props.cursorCol} />
    </Show>
  )
}

function TerminalCursorLine(props: { text: string; cursorCol: number }) {
  const padded = () => props.text.length < props.cursorCol ? props.text.padEnd(props.cursorCol, " ") : props.text
  const before = () => padded().slice(0, props.cursorCol)
  const at = () => padded().slice(props.cursorCol, props.cursorCol + 1) || " "
  const after = () => padded().slice(props.cursorCol + 1)
  return (
    <box flexDirection="row">
      <text fg={theme.muted} wrapMode="none" selectable={true}>{before()}</text>
      <text fg="#0a1014" bg="#ffffff" attributes={TextAttributes.BOLD}>{at()}</text>
      <text fg={theme.muted} wrapMode="none" selectable={true}>{after()}</text>
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
  const activeRole = () => props.slot.modelFamilyEnabled ? (props.slot.activeModelRole ?? "reasoning") : "agent"
  const activeConfig = () => props.slot.modelFamilyEnabled
    ? props.slot.modelFamily[props.slot.activeModelRole ?? "reasoning"]
    : { provider: props.slot.provider, model: props.slot.model, effort: props.slot.effort }
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
        <text fg={theme.dim}>{truncateRight(`${shortModel(activeConfig().model)} (${activeRole()})`, 24)}</text>
        <box flexGrow={1} />
        <text fg={statusColor()}>{s().running ? activeConfig().effort : s().pendingPermission ? "needs approval" : (s().status || "idle")}</text>
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

async function existingPullRequestUrl(worktree: Worktree) {
  try {
    const { stdout } = await execFileAsync("gh", ["pr", "view", worktree.branch_name, "--json", "url", "--jq", ".url"], ghExecOptions(worktree.worktree_path))
    const url = stdout.trim()
    if (url) return url
  } catch (error) {
    if (!ghSupportsJson(error)) return ""
  }
  try {
    const { stdout } = await execFileAsync("gh", ["pr", "view", worktree.branch_name], ghExecOptions(worktree.worktree_path))
    return findUrl(stdout)
  } catch {
    return ""
  }
}

function ghExecOptions(cwd: string) {
  // GH_REPO overrides repository detection in GitHub CLI. Some shells set it
  // to the workspace path, and `gh --repo` also rejects filesystem paths; PR
  // commands should infer the target repository from the worktree's git remote.
  const { GH_REPO: _ghRepo, ...env } = process.env
  return { cwd, env, maxBuffer: 1024 * 1024 }
}

function findUrl(text: string) {
  return text.split(/\r?\n/).map((line) => line.trim()).find((line) => line.startsWith("http://") || line.startsWith("https://")) || ""
}

function ghSupportsJson(error: unknown) {
  const anyError = error as { stderr?: string; stdout?: string; message?: string }
  return !String(anyError?.stderr || anyError?.stdout || anyError?.message || error).includes("unknown flag: --json")
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

function isNavigationEnter(event: { name?: string; sequence?: string }) {
  const name = (event.name || "").toLowerCase()
  return name === "return" || name === "enter" || name === "kpenter" || name === "linefeed" || event.sequence === "\r" || event.sequence === "\n"
}

function isSteerSubmitShortcut(event: { name?: string; sequence?: string; meta?: boolean; super?: boolean; metaKey?: boolean; key?: string }) {
  const name = (event.name || event.key || "").toLowerCase()
  const sequence = event.sequence ?? ""
  const command = Boolean(event.super || event.metaKey)
  const altMeta = Boolean(event.meta && sequence.startsWith("\x1b"))
  const cmdKittyReturn = /^\x1b\[(?:13|57345);9(?::[123])?u$/.test(sequence)
  const altEscReturn = sequence === "\x1b\r" || sequence === "\x1b\n"
  const enter = name === "return" || name === "enter" || name === "kpenter" || name === "linefeed" || sequence === "\r" || sequence === "\n" || altEscReturn || cmdKittyReturn
  return enter && (command || altMeta || cmdKittyReturn || altEscReturn)
}

// Ctrl+N opens a new worktree/session — mirrors the /new command.
function isNewSessionShortcut(event: KeyEvent) {
  return Boolean(event.ctrl) && event.name?.toLowerCase() === "n"
}

function permissionKey(event: KeyEvent) {
  return (event.name || event.sequence || "").toLowerCase()
}

function questionPrintableKey(event: KeyEvent, key: string) {
  if (event.ctrl || event.meta) return ""
  if (event.sequence && event.sequence.length === 1 && event.sequence >= " " && event.sequence !== "\x7f") return event.sequence
  if (key.length === 1 && key >= " " && key !== "\x7f") return key
  if (key === "space") return " "
  return ""
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
  queuedPrompts: QueuedPrompt[]
  pendingPermission?: AppState["pendingPermission"]
  pendingQuestions?: AppState["pendingQuestions"]
  running: boolean
  runningStatus: string
  activityGlyph: string
  permissionSelected: number
  selectPermission: (index: number) => void
  questionIndex: number
  questionAnswers: Record<string, string>
  questionCustomDrafts: Record<string, string>
  questionSelected: Record<string, number>
  questionWarning: string
  setQuestionAnswer: (index: number, value: string) => void
  setQuestionCustomDraft: (index: number, value: string) => void
  setQuestionSelected: (index: number, value: number) => void
  setQuestionIndex: (index: number) => void
  submitQuestions: () => void
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
      <Show when={props.pendingQuestions}>
        {(pending) => (
          <QuestionTimelineItem
            questions={pending().questions}
            index={props.questionIndex}
            answers={props.questionAnswers}
            customDrafts={props.questionCustomDrafts}
            selected={props.questionSelected}
            warning={props.questionWarning}
            setAnswer={props.setQuestionAnswer}
            setCustomDraft={props.setQuestionCustomDraft}
            setSelected={props.setQuestionSelected}
            setIndex={props.setQuestionIndex}
            submit={props.submitQuestions}
          />
        )}
      </Show>
      <Show when={props.pendingPermission}>
        {(request) => <PermissionTimelineItem request={request()} selected={props.permissionSelected} select={props.selectPermission} decide={props.decide} />}
      </Show>
      <Show when={props.running && !props.pendingPermission && !props.pendingQuestions}>
        <AgentWorkingTimelineItem glyph={props.activityGlyph} status={props.runningStatus} />
      </Show>
      <For each={props.queuedPrompts}>
        {(prompt) => <QueuedUserPrompt text={prompt.visiblePrompt} />}
      </For>
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

function horizontalFrame(color: () => string, sides: Array<"top" | "bottom"> = ["top", "bottom"]) {
  return function (this: BoxRenderable, buffer: OptimizedBuffer) {
    const line = "─".repeat(this.width)
    const parsed = parseColor(color())
    if (sides.includes("top")) buffer.drawText(line, this.x, this.y, parsed)
    if (sides.includes("bottom")) buffer.drawText(line, this.x, this.y + this.height - 1, parsed)
  }
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
        paddingTop={1}
        paddingBottom={1}
        renderAfter={horizontalFrame(() => isOpen() ? theme.borderStrong : theme.borderSoft)}
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
            paddingTop={2}
            renderAfter={horizontalFrame(() => isWrite() ? theme.borderStrong : theme.border, ["top"])}
            paddingLeft={1}
            paddingRight={1}
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
  return <PromptCard text={props.text} fg={theme.text} backgroundColor="#3a3a3a" />
}

function QueuedUserPrompt(props: { text: string }) {
  return <PromptCard text={props.text} fg={theme.dim} backgroundColor="#1f2224" />
}

function PromptCard(props: { text: string; fg: string; backgroundColor: string }) {
  return (
    <box width="100%" paddingLeft={2} paddingRight={2}>
      <box
        width="100%"
        backgroundColor={props.backgroundColor}
        paddingLeft={1}
        paddingRight={1}
      >
        <text fg={props.fg} selectable={true} selectionBg={theme.selectionBg} selectionFg={theme.text}>{props.text}</text>
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
        paddingTop={1}
        paddingBottom={1}
        renderAfter={horizontalFrame(() => theme.borderSoft)}
        paddingLeft={1}
        paddingRight={1}
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

function QuestionTimelineItem(props: {
  questions: QuestionItem[]
  index: number
  answers: Record<string, string>
  customDrafts: Record<string, string>
  selected: Record<string, number>
  warning: string
  setAnswer: (index: number, value: string) => void
  setCustomDraft: (index: number, value: string) => void
  setSelected: (index: number, value: number) => void
  setIndex: (index: number) => void
  submit: () => void
}) {
  return (
    <box width="100%" justifyContent="center" paddingLeft={1} paddingRight={1}>
      <QuestionPanel {...props} />
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
            <box
              backgroundColor={theme.panelSoft}
              paddingLeft={1}
              paddingRight={1}
              paddingTop={2}
              paddingBottom={2}
              renderAfter={horizontalFrame(() => theme.borderStrong)}
            >
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
  activeSkills: string[]
  inputRef: (ref: TextareaRenderable) => void
  draft: () => string
  inputWidth: number
  setDraft: (value: string) => void
  submit: (steer?: boolean) => void
  palette: () => PaletteKind
  paletteItems: () => readonly PaletteItem[]
  skillsStatus: string
  selected: () => number
  moveSelection: (delta: number) => void
  dismissPalette: () => void
  acceptPalette: (insertDirectory?: boolean) => void
  choosePalette: (index: number) => void
  openPalette: (kind: PaletteKind) => void
  insertPromptNewline: () => void
  navigatePromptHistory: (direction: HistoryDirection) => boolean
  notice: ComposerNotice
  pasteFromClipboard: () => Promise<void>
  insertPromptText: (text: string) => void
  deletePromptPlaceholder: (direction: "backward" | "forward") => boolean
}) {
  let textarea!: TextareaRenderable
  const showActivity = () => Boolean(props.state.pendingPermission) || props.notice.tone !== "muted"
  const composerPlaceholder = (state: AppState) => state.pendingPermission ? "approval required: press 1, 2, or 3" : state.running ? "Enter queues • Cmd+Enter steers" : props.activeSkills.length ? `Ask INDUCTOR with ${props.activeSkills.join(", ")}...` : "Ask INDUCTOR..."
  const inputRows = createMemo(() => promptVisualRows(props.draft(), props.inputWidth))
  return (
    <box flexShrink={0} flexDirection="column" paddingLeft={2} paddingRight={2} paddingBottom={1}>
      <Show when={props.palette()}>
        {(kind) => <Palette kind={kind()} items={props.paletteItems()} selected={props.selected()} skillsStatus={props.skillsStatus} choose={props.choosePalette} />}
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
        borderColor={props.state.pendingQuestions ? theme.cyan : props.state.pendingPermission ? theme.orange : theme.railActive}
        paddingLeft={1}
        paddingRight={1}
        paddingTop={1}
        paddingBottom={1}
        onMouseUp={() => textarea.focus()}
      >
        <box width="100%" height={inputRows()} flexDirection="row" alignItems="center">
          <textarea
            width="100%"
            alignSelf="center"
            minHeight={inputRows()}
            maxHeight={inputRows()}
            placeholder={composerPlaceholder(props.state)}
            placeholderColor={theme.dim}
            textColor={theme.text}
            focusedTextColor={theme.text}
            focusedBackgroundColor={theme.surface3}
            cursorColor={theme.cyan}
            cursorStyle={{ style: "block", blinking: false }}
            selectionBg={theme.selectionBg}
            selectionFg={theme.text}
            keyBindings={[
              { name: "return", action: "submit" },
              { name: "kpenter", action: "submit" },
              { name: "linefeed", action: "submit" },
              { name: "j", ctrl: true, action: "newline" },
            ]}
            onContentChange={() => props.setDraft(textarea.plainText)}
            onSubmit={() => props.submit(false)}
            onPaste={async (event: { bytes?: Uint8Array; preventDefault(): void }) => {
              const text = decodePasteBytes(event.bytes).replace(/\r\n/g, "\n").replace(/\r/g, "\n")
              if (!text.trim()) {
                event.preventDefault()
                await props.pasteFromClipboard()
                return
              }

              event.preventDefault()
              props.insertPromptText(text)
            }}
            onKeyDown={(event: KeyEvent) => {
              const key = event.name
              const normalized = key?.toLowerCase()
              const ctrl = Boolean(event.ctrl)
              const meta = Boolean(event.meta || event.super)
              const permissionNav = isNavigationEnter(event) || key === "up" || key === "down"
              if ((props.state.pendingPermission || props.state.pendingQuestions) && permissionNav) return
              if (!props.palette() && isSteerSubmitShortcut(event)) {
                event.preventDefault()
                event.stopPropagation?.()
                props.submit(true)
                return
              }
              if (props.palette() && isEscape(event)) {
                event.preventDefault()
                event.stopPropagation?.()
                props.dismissPalette()
                return
              }
              if (props.palette() && key === "up") {
                event.preventDefault()
                event.stopPropagation?.()
                props.moveSelection(-1)
                return
              }
              if (props.palette() && key === "down") {
                event.preventDefault()
                event.stopPropagation?.()
                props.moveSelection(1)
                return
              }
              if (props.palette() && isNavigationEnter(event)) {
                event.preventDefault()
                event.stopPropagation?.()
                props.acceptPalette(Boolean(meta || ctrl))
                return
              }
              if (props.palette() === "models") {
                event.preventDefault()
                event.stopPropagation?.()
                props.dismissPalette()
                return
              }
              if (!props.palette() && (meta || ctrl) && normalized === "v") {
                event.preventDefault()
                event.stopPropagation?.()
                void props.pasteFromClipboard()
                return
              }
              if (!props.palette() && ctrl && normalized === "j") {
                event.preventDefault()
                event.stopPropagation?.()
                props.insertPromptNewline()
                return
              }
              if (!props.palette() && (key === "Backspace" || key === "backspace")) {
                if (props.deletePromptPlaceholder("backward")) {
                  event.preventDefault()
                  event.stopPropagation?.()
                }
                return
              }
              if (!props.palette() && (key === "Delete" || key === "delete")) {
                if (props.deletePromptPlaceholder("forward")) {
                  event.preventDefault()
                  event.stopPropagation?.()
                }
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
                props.submit(Boolean(meta))
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
  skillsStatus: string
  choose: (index: number) => void
}) {
  const maxRows = () => props.kind === "models" || props.kind === "model_family" ? 10 : 14
  const isSkillPalette = () => props.kind === "skills"
  const startIndex = () => {
    const rows = maxRows()
    if (props.items.length <= rows) return 0
    return Math.min(Math.max(0, props.selected - Math.floor(rows / 2)), props.items.length - rows)
  }
  const visibleItems = () => props.items.slice(startIndex(), startIndex() + maxRows())
  const hiddenBefore = () => startIndex()
  const hiddenAfter = () => Math.max(0, props.items.length - startIndex() - visibleItems().length)
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
      <Show when={hiddenBefore() > 0}>
        <text fg={theme.dim}>  ↑ {hiddenBefore()} more</text>
      </Show>
      <Show when={props.items.length === 0}>
        <text fg={theme.muted}>
          {emptyPaletteMessage(props.kind, props.skillsStatus)}
        </text>
      </Show>
      <For each={visibleItems()}>
        {(item, index) => {
          const absoluteIndex = () => startIndex() + index()
          const selected = () => absoluteIndex() === props.selected
          return (
            <box
              flexDirection="row"
              backgroundColor={selected() ? theme.paletteSelected : theme.palette}
              paddingLeft={1}
              paddingTop={isSkillPalette() ? 1 : 0}
              paddingBottom={isSkillPalette() ? 1 : 0}
              onMouseUp={() => props.choose(absoluteIndex())}
            >
              <text width={18} fg={paletteItemLabelColor(item, selected())} attributes={selected() ? TextAttributes.BOLD : undefined}>
                {paletteItemLabel(item)}
              </text>
              <text fg={selected() ? theme.text : theme.muted}>
                {paletteItemDescription(item)}
              </text>
            </box>
          )
        }}
      </For>
      <Show when={hiddenAfter() > 0}>
        <text fg={theme.dim}>  ↓ {hiddenAfter()} more · use ↑↓ to scroll</text>
      </Show>
    </box>
  )
}

function emptyPaletteMessage(kind: PaletteKind, skillsStatus: string) {
  if (kind === "skills") {
    if (skillsStatus === "loading") return "  loading skills…"
    if (skillsStatus) return `  could not load skills: ${skillsStatus}`
    return "  no skills found — create one with /skill"
  }
  return "  no matches"
}

function paletteItemLabelColor(item: PaletteItem, selected: boolean) {
  if (isSkillChoice(item)) return theme.skillOrange
  return selected ? theme.cyan : theme.text
}

function isSkillChoice(item: PaletteItem): item is SkillChoice {
  return "source" in item && "path" in item
}

function paletteItemLabel(item: PaletteItem) {
  return "label" in item ? item.label : item.name
}

function paletteItemDescription(item: PaletteItem) {
  if ("efforts" in item) return item.group
  if ("source" in item) return `${item.source} · ${item.description || item.path}`
  if ("description" in item) return item.description
  return ""
}

function findActiveSkillMention(value: string): SkillMentionState | undefined {
  const triggerStart = value.lastIndexOf("$")
  if (triggerStart < 0) return undefined
  if (triggerStart > 0 && !/\s/.test(value[triggerStart - 1])) return undefined

  const token = value.slice(triggerStart)
  if (/\s/.test(token)) return undefined
  const query = token.slice(1)
  if (query.includes("/")) return undefined
  return { triggerStart, token, query }
}

function replaceSkillMention(value: string, mention: SkillMentionState, choice: SkillChoice) {
  return `${value.slice(0, mention.triggerStart)}$${choice.name} ${value.slice(mention.triggerStart + mention.token.length)}`
}

function applySkillHighlights(textarea: TextareaRenderable | undefined, value: string, choices: readonly SkillChoice[], style: SyntaxStyle, styleId: number, hlRef: number) {
  if (!textarea) return
  textarea.syntaxStyle = style
  textarea.removeHighlightsByRef(hlRef)

  for (const span of skillMentionSpans(value, choices)) {
    textarea.addHighlightByCharRange({ start: span.start, end: span.end, styleId, hlRef, priority: 100 })
  }
}

function extractSkillMentions(value: string, choices: readonly SkillChoice[]) {
  return uniqueStrings(skillMentionSpans(value, choices).map((span) => span.name))
}

function skillPlaceholders(value: string, choices: readonly SkillChoice[]): PromptPlaceholder[] {
  return skillMentionSpans(value, choices).map((span) => ({ label: value.slice(span.start, span.end), replacement: span.name }))
}

function skillMentionSpans(value: string, choices: readonly SkillChoice[]) {
  if (choices.length === 0) return []
  const spans: { start: number; end: number; name: string }[] = []
  const sorted = [...choices]
    .map((choice) => choice.name)
    .filter(Boolean)
    .sort((a, b) => b.length - a.length)

  for (let index = 0; index < value.length; index += 1) {
    if (value[index] !== "$") continue
    if (index > 0 && !/\s/.test(value[index - 1])) continue

    for (const name of sorted) {
      const end = index + 1 + name.length
      if (value.slice(index + 1, end) !== name) continue
      const next = value[end]
      if (next && !/\s/.test(next)) continue
      spans.push({ start: index, end, name })
      index = end - 1
      break
    }
  }

  return spans
}

function createWorkspaceSkill(workspace: string, name: string, description: string, body: string) {
  const slug = sanitizeSkillName(name)
  if (!slug) throw new Error("invalid skill name")
  const filePath = path.join(workspace, ".inductor", "skills", slug, "SKILL.md")
  mkdirSync(path.dirname(filePath), { recursive: true })
  writeFileSync(filePath, `---\nname: ${slug}\ndescription: ${yamlString(description)}\n---\n\n# ${slug}\n\n${body.trim()}\n`)
  return filePath
}

function sanitizeSkillName(name: string) {
  return name.trim().toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "")
}

function yamlString(value: string) {
  return JSON.stringify(value)
}

function uniqueStrings(values: readonly string[]) {
  return Array.from(new Set(values))
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
          <TodosPanel todos={props.state.todos} />
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


function TodosPanel(props: { todos: AppState["todos"] }) {
  return (
    <box flexDirection="column" gap={1}>
      <text fg={theme.cyan}>TO-DOS</text>
      <Show when={props.todos.length > 0} fallback={<text fg={theme.dim}>No todos yet</text>}>
        <For each={props.todos}>
          {(todo) => (
            <box flexDirection="row" gap={1} alignItems="flex-start">
              <text width={2} flexShrink={0} fg={todo.status === "completed" ? theme.green : todo.status === "in_progress" ? theme.cyan : theme.muted}>
                {todo.status === "completed" ? "✓" : todo.status === "in_progress" ? "●" : "○"}
              </text>
              <box flexGrow={1} minWidth={0}>
                <text fg={todo.status === "completed" ? theme.dim : theme.text} wrapMode="word">
                  {todo.content}
                </text>
              </box>
            </box>
          )}
        </For>
      </Show>
    </box>
  )
}

function CustomAnswerInput(props: { text: string; active: boolean }) {
  return (
    <box flexDirection="row" minHeight={1}>
      <Show
        when={props.text}
        fallback={
          <>
            <Show when={props.active}>
              <text fg={theme.cyan} bg={theme.cyan}> </text>
            </Show>
            <text fg={theme.dim} wrapMode="none">Type custom answer...</text>
          </>
        }
      >
        {(text) => (
          <>
            <text fg={theme.text} wrapMode="none">{text()}</text>
            <Show when={props.active}>
              <text fg={theme.cyan} bg={theme.cyan}> </text>
            </Show>
          </>
        )}
      </Show>
    </box>
  )
}

function QuestionPanel(props: {
  questions: QuestionItem[]
  index: number
  answers: Record<string, string>
  customDrafts: Record<string, string>
  selected: Record<string, number>
  warning: string
  setAnswer: (index: number, value: string) => void
  setCustomDraft: (index: number, value: string) => void
  setSelected: (index: number, value: number) => void
  setIndex: (index: number) => void
  submit: () => void
}) {
  const current = () => props.questions[Math.min(props.index, Math.max(0, props.questions.length - 1))]
  const options = () => current()?.options ?? []
  const selected = () => Math.min(props.selected[String(props.index)] ?? 0, Math.max(0, options().length))
  return (
    <box width="88%" flexDirection="column" gap={1} border borderStyle="rounded" borderColor={theme.cyan} paddingLeft={2} paddingRight={2} paddingTop={1} paddingBottom={1} backgroundColor={theme.panelSoft}>
      <box flexDirection="row" justifyContent="space-between">
        <text fg={theme.cyan} attributes={TextAttributes.BOLD}>QUESTION {props.index + 1}/{props.questions.length}</text>
        <text fg={theme.muted}>←/→ switch · ↑/↓ option · type custom below</text>
      </box>
      <text fg={theme.text} wrapMode="word">Q{props.index + 1} {current()?.question ?? "Question"}</text>
      <For each={options()}>
        {(option, optionIndex) => {
          const active = () => selected() === optionIndex()
          const label = () => String.fromCharCode(97 + optionIndex())
          return (
            <box flexDirection="column" backgroundColor={active() ? theme.paletteSelected : theme.row} paddingLeft={1} paddingRight={1} onMouseUp={() => { props.setSelected(props.index, optionIndex()); props.setAnswer(props.index, option.label ?? "") }}>
              <text fg={active() ? theme.cyan : theme.text} attributes={active() ? TextAttributes.BOLD : undefined}>{active() ? "› " : "  "}{label()}) {option.label}{current()?.recommended === option.label ? " (recommended)" : ""}</text>
              <Show when={option.description}><text fg={theme.muted} wrapMode="word">{option.description}</text></Show>
              <Show when={option.pros}><text fg={theme.green} wrapMode="word">pro: {option.pros}</text></Show>
              <Show when={option.cons}><text fg={theme.red} wrapMode="word">con: {option.cons}</text></Show>
            </box>
          )
        }}
      </For>
      <box
        flexDirection="column"
        backgroundColor={selected() === options().length ? theme.paletteSelected : theme.row}
        paddingLeft={1}
        paddingRight={1}
        onMouseUp={() => props.setSelected(props.index, options().length)}
      >
        <text fg={selected() === options().length ? theme.cyan : theme.text} attributes={selected() === options().length ? TextAttributes.BOLD : undefined}>
          {selected() === options().length ? "› " : "  "}{String.fromCharCode(97 + options().length)}) Custom answer
        </text>
        <text fg={theme.muted} wrapMode="word">Select this, then type your answer here:</text>
        <box
          width="100%"
          border
          borderStyle="rounded"
          borderColor={selected() === options().length ? theme.cyan : theme.border}
          paddingLeft={1}
          paddingRight={1}
          paddingTop={1}
          paddingBottom={1}
          marginTop={1}
          onMouseUp={() => props.setSelected(props.index, options().length)}
        >
          <CustomAnswerInput text={props.customDrafts[String(props.index)] ?? ""} active={selected() === options().length} />
        </box>
      </box>
      <Show when={props.warning}><text fg={theme.red}>{props.warning}</text></Show>
      <text fg={theme.muted}>Enter accepts the highlighted choice and advances; on the last question, Enter submits.</text>
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

function roleLabel(role: ModelRole) {
  if (role === "reasoning") return "Reasoning"
  if (role === "executor") return "Executor"
  return "Reviewer"
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
  const type = operationKind(op)
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
  const operationDiff = diffFromStructuredPatchOperation(path, op)
  if (operationDiff) return operationDiff
  const oldText = stringField(op, ["old", "old_text", "before"])
  const newText = stringField(op, ["new", "new_text", "content", "after"])
  return oldText || newText ? createUnifiedPatchFromContent(path, oldText ?? "", newText ?? "") : undefined
}

function diffFromStructuredPatchOperation(path: string, op: Record<string, unknown>) {
  const type = operationKind(op)
  if (type === "insert_before" || type === "insert_after") {
    const content = stringField(op, ["content", "new"]) ?? ""
    const expectedLine = stringField(op, ["expected_line"])
    if (!expectedLine) return createUnifiedPatchFromContent(path, "", content)
    const context = ensureTrailingNewline(expectedLine)
    const inserted = ensureTrailingNewline(content)
    return createUnifiedPatchFromContent(
      path,
      context,
      type === "insert_before" ? `${inserted}${context}` : `${context}${inserted}`,
    )
  }
  if (type === "add_file") {
    return createUnifiedPatchFromContent(path, "", stringField(op, ["content", "new"]) ?? "")
  }
  if (type === "delete_file") {
    return createUnifiedPatchFromContent(path, stringField(op, ["old", "content"]) ?? "", "")
  }
  return undefined
}

function operationKind(record: Record<string, unknown>) {
  return stringField(record, ["op", "type"])
}

function ensureTrailingNewline(value: string) {
  return value.endsWith("\n") ? value : `${value}\n`
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
  const operationPath = operationPathFromToolInput(json)
  if (operationPath) return operationPath
  const patch = json?.patch
  if (typeof patch === "string") return patchFilesFromUnifiedPatch(patch)[0]?.path
  if (Array.isArray(patch)) {
    const first = patch.find((file) => file && typeof file === "object" && typeof (file as Record<string, unknown>).path === "string") as Record<string, unknown> | undefined
    return typeof first?.path === "string" ? first.path : undefined
  }
  const diff = stringField(json, ["diff"])
  return diff ? patchFilesFromUnifiedPatch(diff)[0]?.path : undefined
}

function operationPathFromToolInput(json: Record<string, unknown> | undefined) {
  const operations = Array.isArray(json?.operations) ? json.operations : []
  const paths = operations
    .map((operation) => {
      if (!operation || typeof operation !== "object") return undefined
      return stringField(operation as Record<string, unknown>, ["path", "filepath", "file_path", "target", "filename", "from", "to"])
    })
    .filter((path): path is string => Boolean(path))
  const uniquePaths = [...new Set(paths)]
  if (uniquePaths.length === 0) return undefined
  if (uniquePaths.length === 1) return uniquePaths[0]
  return `${uniquePaths[0]} +${uniquePaths.length - 1}`
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

function promptVisualRows(value: string, width: number) {
  const usableWidth = Math.max(1, Math.floor(width))
  const rows = value.split("\n").reduce((total, line) => {
    const visualWidth = stringWidth(line)
    return total + Math.max(1, Math.ceil(visualWidth / usableWidth))
  }, 0)
  return Math.max(1, rows)
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
