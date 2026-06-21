import { describe, expect, test } from "bun:test"
import { createUnifiedPatchFromContent, normalizeDiffForRendering, normalizeUnifiedPatch, patchFilesFromUnifiedPatch } from "../src/diff_patch"

describe("diff patch helpers", () => {
  test("wraps bare apply-patch hunks in unified file headers", () => {
    const patch = "@@ -1 +1 @@\n-old\n+new\n"

    expect(normalizeUnifiedPatch("src/main.ts", patch)).toBe("--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1 +1 @@\n-old\n+new\n")
  })

  test("keeps deleted-file patches renderable as red removals", () => {
    const patch = [
      "diff --git a/old.txt b/old.txt",
      "deleted file mode 100644",
      "index 7898192..0000000",
      "--- a/old.txt",
      "+++ b/old.txt",
      "@@ -1,2 +0,0 @@",
      "-one",
      "-two",
      "",
    ].join("\n")

    expect(normalizeDiffForRendering(patch)).toContain("+++ /dev/null\told.txt")
    expect(normalizeDiffForRendering(patch)).toContain("-one")
  })

  test("creates deletion-only patches from old/new content", () => {
    const patch = createUnifiedPatchFromContent("old.txt", "one\ntwo\n", "")

    expect(patch).toContain("-one")
    expect(patch).toContain("-two")
  })

  test("extracts file summaries from unified apply patches", () => {
    const files = patchFilesFromUnifiedPatch("--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1 +1 @@\n-old\n+new\n")

    expect(files).toEqual([
      expect.objectContaining({ path: "src/main.ts", additions: 1, deletions: 1, diff: expect.stringContaining("+new") }),
    ])
  })
})
