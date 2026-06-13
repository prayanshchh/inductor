import { describe, expect, test } from "bun:test"
import { createReviewedHunks, hunkReviewSummary, parseReviewHunks, setHunkStatus } from "../src/hunk_review"

describe("hunk review helpers", () => {
  const diff = `--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!("old");
+    println!("new");
 }
@@ -8,2 +8,3 @@
 mod tests {
+    #[test]
 }
`

  test("parses hunks with addition and deletion counts", () => {
    const hunks = parseReviewHunks(diff)

    expect(hunks).toHaveLength(2)
    expect(hunks[0]).toMatchObject({ id: "1", additions: 1, deletions: 1 })
    expect(hunks[1]).toMatchObject({ id: "2", additions: 1, deletions: 0 })
  })

  test("tracks accepted and rejected hunk decisions", () => {
    let hunks = createReviewedHunks(diff)
    hunks = setHunkStatus(hunks, "1", "accepted")
    hunks = setHunkStatus(hunks, "2", "rejected")

    expect(hunkReviewSummary(hunks)).toEqual({ total: 2, accepted: 1, rejected: 1, pending: 0 })
  })
})
