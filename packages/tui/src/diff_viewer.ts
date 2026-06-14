import { chmodSync, mkdirSync, writeFileSync } from "node:fs"
import { homedir } from "node:os"
import path from "node:path"
import type { ModifiedFile } from "./state"

type TerminalKind = "terminal" | "iterm" | "warp" | "generic"

export type TerminalApp = {
  kind: TerminalKind
  appName: string
}

export type DiffViewerFiles = {
  patchPath: string
  scriptPath: string
}

export function openExternalDiffViewer(workspace: string, file: ModifiedFile, env: Record<string, string | undefined> = process.env) {
  const { scriptPath } = createDiffViewerFiles(workspace, file)
  openTerminalScript(workspace, scriptPath, detectTerminalApp(env))
}

export function createDiffViewerFiles(workspace: string, file: ModifiedFile): DiffViewerFiles {
  const dir = path.join(workspace, ".inductor", "diff-viewer")
  mkdirSync(dir, { recursive: true })

  const basename = safeDiffName(file.file)
  const patchPath = path.join(dir, `${basename}.diff`)
  const scriptPath = path.join(dir, `${basename}.command`)
  const patch = file.diff?.trim()
    ? file.diff
    : `No captured patch was available for ${file.file}.\n\nShowing live git diff if this file still has worktree changes.\n`

  writeFileSync(patchPath, patch)
  writeFileSync(scriptPath, diffViewerScript(workspace, file.file, patchPath))
  chmodSync(scriptPath, 0o755)

  return { patchPath, scriptPath }
}

export function detectTerminalApp(env: Record<string, string | undefined> = process.env): TerminalApp {
  const termProgram = env.TERM_PROGRAM ?? ""
  const lcTerminal = env.LC_TERMINAL ?? ""
  const bundleId = env.__CFBundleIdentifier ?? ""
  const haystack = `${termProgram} ${lcTerminal} ${bundleId}`.toLowerCase()

  if (haystack.includes("warp")) return { kind: "warp", appName: "Warp" }
  if (haystack.includes("iterm") || bundleId === "com.googlecode.iterm2") {
    return { kind: "iterm", appName: "iTerm2" }
  }
  if (termProgram === "Apple_Terminal" || bundleId === "com.apple.Terminal") {
    return { kind: "terminal", appName: "Terminal" }
  }

  const genericApp = genericTerminalAppName(termProgram)
  if (genericApp) return { kind: "generic", appName: genericApp }

  return { kind: "terminal", appName: "Terminal" }
}

function openTerminalScript(workspace: string, scriptPath: string, terminal: TerminalApp) {
  const command = `zsh ${shellQuote(scriptPath)}`

  if (terminal.kind === "terminal") {
    runAppleScript(terminalScript(command))
    return
  }

  if (terminal.kind === "iterm") {
    runAppleScript(iTermScript(command))
    return
  }

  if (terminal.kind === "warp") {
    openWarpTabConfig(workspace, command)
    return
  }

  openWithApp(terminal.appName, scriptPath)
}

function openWarpTabConfig(workspace: string, command: string) {
  const configName = "inductor_diff_viewer"
  const configDir = path.join(homedir(), ".warp", "tab_configs")
  mkdirSync(configDir, { recursive: true })
  writeFileSync(path.join(configDir, `${configName}.toml`), createWarpTabConfig(workspace, command))
  Bun.spawn(["open", `warp://tab_config/${configName}`], {
    stdout: "ignore",
    stderr: "ignore",
  }).unref()
}

export function createWarpTabConfig(workspace: string, command: string) {
  return [
    `name = "Inductor Diff Viewer"`,
    `title = "inductor diff"`,
    `color = "cyan"`,
    "",
    "[[panes]]",
    `id = "main"`,
    `type = "terminal"`,
    `directory = ${tomlString(workspace)}`,
    `commands = [${tomlString(command)}]`,
    `is_focused = true`,
    "",
  ].join("\n")
}

function openWithApp(appName: string, scriptPath: string) {
  Bun.spawn(["open", "-a", appName, scriptPath], {
    stdout: "ignore",
    stderr: "ignore",
  }).unref()
}

function runAppleScript(script: string) {
  Bun.spawn(["osascript", "-e", script], {
    stdout: "ignore",
    stderr: "ignore",
  }).unref()
}

function terminalScript(command: string) {
  return [
    `tell application id "com.apple.Terminal"`,
    `activate`,
    `if not (exists window 1) then reopen`,
    `do script "${appleScriptString(command)}" in front window`,
    `end tell`,
  ].join("\n")
}

function iTermScript(command: string) {
  return [
    `tell application id "com.googlecode.iterm2"`,
    `activate`,
    `if (count of windows) = 0 then`,
    `create window with default profile command "${appleScriptString(command)}"`,
    `else`,
    `tell current window`,
    `create tab with default profile command "${appleScriptString(command)}"`,
    `end tell`,
    `end if`,
    `end tell`,
  ].join("\n")
}

function diffViewerScript(workspace: string, file: string, patchPath: string) {
  return [
    "#!/bin/zsh",
    "set -u",
    `cd ${shellQuote(workspace)} || exit 1`,
    `printf '\\033]0;%s\\007' ${shellQuote(`inductor diff: ${file}`)}`,
    "{",
    `  printf '%s\\n\\n' ${shellQuote(`INDUCTOR DIFF: ${file}`)}`,
    `  if git rev-parse --is-inside-work-tree >/dev/null 2>&1 && ! git diff --quiet HEAD -- ${shellQuote(file)} 2>/dev/null; then`,
    `    git diff --color=always HEAD -- ${shellQuote(file)}`,
    "  else",
    `    awk ${shellQuote(colorPatchAwk())} ${shellQuote(patchPath)}`,
    "  fi",
    "} | LESS=R less -R",
    "",
  ].join("\n")
}

function colorPatchAwk() {
  return [
    "BEGIN { esc = sprintf(\"%c\", 27); reset = esc \"[0m\"; green = esc \"[32m\"; red = esc \"[31m\"; cyan = esc \"[36m\"; dim = esc \"[2m\" }",
    "/^diff --git / { print dim $0 reset; next }",
    "/^@@/ { print cyan $0 reset; next }",
    "/^\\+\\+\\+ / { print cyan $0 reset; next }",
    "/^--- / { print cyan $0 reset; next }",
    "/^\\+/ { print green $0 reset; next }",
    "/^-/ { print red $0 reset; next }",
    "{ print }",
  ].join("\n")
}

function genericTerminalAppName(termProgram: string) {
  if (!termProgram || termProgram === "Apple_Terminal") return undefined
  if (/^(vscode|cursor|tmux|screen)$/i.test(termProgram)) return undefined
  return termProgram.replace(/\.app$/i, "")
}

function safeDiffName(file: string) {
  const safe = file.replace(/[^a-zA-Z0-9._-]+/g, "_").replace(/^_+|_+$/g, "")
  return (safe || "diff").slice(-140)
}

function shellQuote(value: string) {
  return `'${value.replaceAll("'", "'\\''")}'`
}

function appleScriptString(value: string) {
  return value.replaceAll("\\", "\\\\").replaceAll("\"", "\\\"")
}

function tomlString(value: string) {
  return JSON.stringify(value)
}
