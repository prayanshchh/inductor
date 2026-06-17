import { describe, expect, test } from "bun:test"
import { parsePatch } from "diff"
import { createUnifiedPatchFromContent, normalizeDiffForRendering } from "../src/diff_patch"

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

  test("normalizes over-counted trailing context for the OpenTUI diff renderer", () => {
    const patch = `--- a/crates/harness-core/src/lib.rs
+++ b/crates/harness-core/src/lib.rs
@@ -458,6 +458,15 @@ pub enum SessionEvent {
         /// Provider-reported cost in USD, when available (Claude SDK reports it).
         total_cost_usd: Option<f64>,
     },
+    /// Session/worktree metadata changed while the run is still active.
+    MetadataUpdated {
+        session_id: SessionId,
+        display_name: Option<String>,
+        workspace_id: Option<WorkspaceId>,
+        worktree_path: Option<String>,
+        branch_name: Option<String>,
+    },
 }
`

    expect(() => parsePatch(patch)).toThrow("invalid line")
    const normalized = normalizeDiffForRendering(patch)
    expect(() => parsePatch(normalized)).not.toThrow()
    expect(normalized).toContain("@@ -458,4 +458,12 @@")
    expect(normalized).toContain("+    MetadataUpdated {")
  })

  test("normalizes multiple hunks independently", () => {
    const patch = `--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,7 +2,7 @@ use std::{
     error::Error,
     ffi::OsStr,
     fmt, fs,
-    path::{Path, PathBuf},
+    path::{Path, PathBuf},
     process::Command,
 };
@@ -127,6 +127,10 @@ impl WorktreeManager {
         git_stdout(&repo.root, ["branch", "-m", old_branch, new_branch])?;
         Ok(())
     }
+
+    pub fn rename_managed_worktree(
+        &self,
+    ) -> Result<(), GitError> {
`

    expect(() => parsePatch(patch)).toThrow("invalid line")
    const normalized = normalizeDiffForRendering(patch)
    expect(() => parsePatch(normalized)).not.toThrow()
    expect(normalized).toContain("@@ -2,6 +2,6 @@")
    expect(normalized).toContain("@@ -127,3 +127,7 @@")
  })
})
