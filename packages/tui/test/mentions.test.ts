import { describe, expect, test } from "bun:test"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import path from "node:path"
import { findActiveMention, isImagePath, listFileChoices, promptForSubmit, replaceMention, stripToken } from "../src/mentions"

function tempWorkspace() {
  const root = path.join(tmpdir(), `inductor-tui-${Date.now()}-${Math.random().toString(16).slice(2)}`)
  mkdirSync(root, { recursive: true })
  return root
}

describe("file mention helpers", () => {
  test("detects active @ token at the end of the prompt", () => {
    expect(findActiveMention("read @src/li")).toMatchObject({ dir: "src/", query: "li" })
    expect(findActiveMention("email a@b.com")).toBeUndefined()
    expect(findActiveMention("read @src/lib.rs now")).toBeUndefined()
  })

  test("lists dirs first and filters within the active directory", () => {
    const root = tempWorkspace()
    try {
      mkdirSync(path.join(root, "src", "bin"), { recursive: true })
      writeFileSync(path.join(root, "src", "lib.rs"), "")
      writeFileSync(path.join(root, "README.md"), "")

      const rootChoices = listFileChoices(root, findActiveMention("open @s")!)
      expect(rootChoices[0]).toMatchObject({ name: "src/", kind: "dir" })

      const srcChoices = listFileChoices(root, findActiveMention("open @src/l")!)
      expect(srcChoices).toContainEqual(expect.objectContaining({ name: "lib.rs", path: "src/lib.rs" }))
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test("enter descends into dirs while command enter inserts the dir token", () => {
    const mention = findActiveMention("read @sr")!
    const dir = { name: "src/", path: "src/", kind: "dir" as const, description: "directory" }

    expect(replaceMention("read @sr", mention, dir)).toBe("read @src/")
    expect(replaceMention("read @sr", mention, dir, true)).toBe("read @src/ ")
  })

  test("recognizes image paths from pasted terminal tokens", () => {
    expect(isImagePath("screen.png")).toBe(true)
    expect(isImagePath("notes.md")).toBe(false)
    expect(stripToken("'/tmp/screen shot.png',")).toBe("/tmp/screen shot.png")
  })

  test("expands visible image placeholders to hidden attachment mentions on submit", () => {
    const prompt = promptForSubmit("compare [Image #1] with [Image #2]", [
      { label: "[Image #1]", path: ".inductor/attachments/pasted-image-1.png" },
      { label: "[Image #2]", path: ".inductor/attachments/pasted-image-2.png" },
    ])

    expect(prompt).toBe("compare [Image #1] @.inductor/attachments/pasted-image-1.png with [Image #2] @.inductor/attachments/pasted-image-2.png")
  })

  test("expands visible paste placeholders to original pasted text on submit", () => {
    const prompt = promptForSubmit("summarize [Pasted text #1]", [], [
      { label: "[Pasted text #1]", text: "line 1\nline 2" },
    ])

    expect(prompt).toBe("summarize line 1\nline 2")
  })
})
