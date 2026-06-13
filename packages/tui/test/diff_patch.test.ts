import { describe, expect, test } from "bun:test"
import { createUnifiedPatchFromContent } from "../src/diff_patch"

describe("diff patch helpers", () => {
  test("keeps unchanged lines as context when content has additions", () => {
    const patch = createUnifiedPatchFromContent("src/main.rs", "fn main() {\n}\n", "fn main() {\n    println!(\"hi\");\n}\n")

    expect(patch).toContain(" fn main() {")
    expect(patch).toContain("+    println!(\"hi\");")
    expect(patch).toContain(" }")
    expect(patch).not.toContain("-fn main() {")
    expect(patch).not.toContain("+fn main() {")
  })

  test("keeps unchanged lines as context when content has deletions", () => {
    const patch = createUnifiedPatchFromContent("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n", "fn main() {\n}\n")

    expect(patch).toContain(" fn main() {")
    expect(patch).toContain("-    println!(\"hi\");")
    expect(patch).toContain(" }")
    expect(patch).not.toContain("-fn main() {")
    expect(patch).not.toContain("+fn main() {")
  })

  test("returns undefined when content is unchanged", () => {
    expect(createUnifiedPatchFromContent("src/main.rs", "same\n", "same\n")).toBeUndefined()
  })
})
