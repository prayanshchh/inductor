import { mkdtempSync, readFileSync, statSync } from "node:fs"
import { tmpdir } from "node:os"
import path from "node:path"
import { describe, expect, test } from "bun:test"
import { createDiffViewerFiles, createWarpTabConfig, detectTerminalApp } from "../src/diff_viewer"

describe("external diff viewer", () => {
  test("detects the terminal app from common terminal environments", () => {
    expect(detectTerminalApp({ TERM_PROGRAM: "WarpTerminal" })).toEqual({ kind: "warp", appName: "Warp" })
    expect(detectTerminalApp({ TERM_PROGRAM: "iTerm.app" })).toEqual({ kind: "iterm", appName: "iTerm2" })
    expect(detectTerminalApp({ TERM_PROGRAM: "Apple_Terminal" })).toEqual({ kind: "terminal", appName: "Terminal" })
    expect(detectTerminalApp({ TERM_PROGRAM: "WezTerm" })).toEqual({ kind: "generic", appName: "WezTerm" })
  })

  test("creates an executable runner that opens the selected file diff in less", () => {
    const workspace = mkdtempSync(path.join(tmpdir(), "inductor-diff-viewer-"))
    const { patchPath, scriptPath } = createDiffViewerFiles(workspace, {
      file: "src/main.ts",
      additions: 1,
      deletions: 1,
      diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1 +1 @@\n-old\n+new\n",
    })

    expect(readFileSync(patchPath, "utf8")).toContain("+new")

    const script = readFileSync(scriptPath, "utf8")
    expect(script).toContain("INDUCTOR DIFF: src/main.ts")
    expect(script).toContain("git diff --color=always HEAD -- 'src/main.ts'")
    expect(script).toContain("awk ")
    expect(script).toContain("LESS=R less -R")
    expect(statSync(scriptPath).mode & 0o111).not.toBe(0)
  })

  test("creates a Warp tab config that runs the diff command in the workspace", () => {
    const config = createWarpTabConfig("/Users/me/project", "zsh '/Users/me/project/.inductor/diff-viewer/src.command'")

    expect(config).toContain('name = "Inductor Diff Viewer"')
    expect(config).toContain('directory = "/Users/me/project"')
    expect(config).toContain('commands = ["zsh \'/Users/me/project/.inductor/diff-viewer/src.command\'"]')
  })
})
