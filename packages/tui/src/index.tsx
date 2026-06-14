/** @jsxImportSource @opentui/solid */
import { createCliRenderer, type CliRenderer } from "@opentui/core"
import { render } from "@opentui/solid"
import { App } from "./app"

type Args = {
  backendBin: string
  workspace: string
  provider: string
  model?: string
  approval: string
  repoRoot: string
}

const args = parseArgs(process.argv.slice(2))
let rawCtrlCHandler: (() => void) | undefined
const renderer = await createCliRenderer({
  externalOutputMode: "passthrough",
  targetFps: 60,
  exitOnCtrlC: false,
  clearOnShutdown: false,
  backgroundColor: "transparent",
  useKittyKeyboard: {},
  autoFocus: true,
  useMouse: true,
  consoleOptions: {
    keyBindings: [{ name: "y", ctrl: true, action: "copy-selection" }],
  },
  prependInputHandlers: [
    (sequence) => {
      if (sequence !== "\x03" || !rawCtrlCHandler) return false
      rawCtrlCHandler()
      return true
    },
  ],
})
let exitRequested = false
function exitApp(code = 0) {
  if (exitRequested) return
  exitRequested = true
  renderer.destroy()
  setTimeout(() => process.exit(code), 0)
}

process.once("SIGHUP", () => exitApp(129))
process.once("SIGINT", () => exitApp(130))
process.once("SIGTERM", () => exitApp(143))

installSelectionClipboard(renderer)
void render(() => (
  <App
    {...args}
    exitApp={exitApp}
    registerCtrlCHandler={(handler) => {
      rawCtrlCHandler = handler
    }}
  />
), renderer)

type ClipboardSelection = {
  anchor: { x: number; y: number }
  focus: { x: number; y: number }
  getSelectedText(): string
}

function installSelectionClipboard(renderer: CliRenderer) {
  renderer.on("selection", (selection: ClipboardSelection) => {
    if (selection.anchor.x === selection.focus.x && selection.anchor.y === selection.focus.y) return
    const text = selection.getSelectedText()
    if (!text.trim()) return
    if (renderer.copyToClipboardOSC52(text)) return
    void import("clipboardy")
      .then(({ default: clipboardy }) => clipboardy.write(text))
      .catch(() => undefined)
  })
}

function parseArgs(raw: string[]): Args {
  const values = new Map<string, string>()
  for (let index = 0; index < raw.length; index += 1) {
    const part = raw[index]
    if (!part.startsWith("--")) continue
    const key = part.slice(2)
    const value = raw[index + 1]
    if (value && !value.startsWith("--")) {
      values.set(key, value)
      index += 1
    } else {
      values.set(key, "true")
    }
  }

  return {
    backendBin: values.get("backend-bin") ?? "inductor",
    workspace: values.get("workspace") ?? process.cwd(),
    provider: values.get("provider") ?? "claude",
    model: values.get("model") || undefined,
    approval: values.get("approval") ?? "mutating",
    repoRoot: values.get("repo-root") ?? process.cwd(),
  }
}
