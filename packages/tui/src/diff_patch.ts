import { createTwoFilesPatch } from "diff"

export type PatchFileSummary = {
  path: string
  additions: number
  deletions: number
  diff: string
}

export function createUnifiedPatchFromContent(filePath: string, oldText = "", newText = "") {
  if (oldText === newText) return undefined

  return createTwoFilesPatch(
    `a/${filePath}`,
    `b/${filePath}`,
    oldText,
    newText,
    undefined,
    undefined,
    { context: 3, stripTrailingCr: true },
  )
}

export function normalizeDiffForRendering(diff: string) {
  return normalizeHunkCounts(normalizeDeletedFilePatch(diff))
}

export function normalizeUnifiedPatch(filePath: string, patch: string) {
  const trimmed = patch.trimStart()
  if (trimmed.startsWith("diff --git ") || trimmed.startsWith("--- ") || trimmed.startsWith("Index: ")) return patch
  if (trimmed.startsWith("@@")) return `--- a/${filePath}\n+++ b/${filePath}\n${patch}`
  return patch
}

export function patchFilesFromUnifiedPatch(patch: string): PatchFileSummary[] {
  const normalized = normalizeGitPatchForParsing(patch)
  const lines = normalized.split(/\r?\n/)
  const files: PatchFileSummary[] = []
  let index = 0

  while (index < lines.length) {
    if (!lines[index].startsWith("--- ")) {
      index += 1
      continue
    }

    const oldHeader = lines[index]
    const newHeader = lines[index + 1]
    if (!newHeader?.startsWith("+++ ")) {
      index += 1
      continue
    }

    const path = patchPathFromHeaders(oldHeader, newHeader)
    const start = index
    index += 2
    let additions = 0
    let deletions = 0

    while (index < lines.length && !isFilePatchBoundary(lines[index])) {
      const line = lines[index]
      if (line.startsWith("+") && !line.startsWith("+++")) additions += 1
      if (line.startsWith("-") && !line.startsWith("---")) deletions += 1
      index += 1
    }

    const fileDiff = lines.slice(start, index).join("\n")
    if (path && (additions > 0 || deletions > 0 || fileDiff.includes("@@"))) {
      files.push({ path, additions, deletions, diff: fileDiff })
    }
  }

  return files
}

function normalizeGitPatchForParsing(patch: string) {
  const lines = patch.split(/\r?\n/)
  const out: string[] = []
  for (const line of lines) {
    if (line.startsWith("diff --git ") || line.startsWith("index ") || line.startsWith("new file mode ") || line.startsWith("deleted file mode ")) continue
    out.push(line)
  }
  return out.join("\n")
}

function patchPathFromHeaders(oldHeader: string, newHeader: string) {
  const oldPath = cleanPatchPath(oldHeader.slice(4).trim())
  const newPath = cleanPatchPath(newHeader.slice(4).trim())
  return newPath !== "/dev/null" ? newPath : oldPath
}

function cleanPatchPath(path: string) {
  const first = path.split(/\s+/)[0] ?? path
  if (first === "/dev/null") return first
  return first.replace(/^[ab]\//, "")
}

function isFilePatchBoundary(line: string) {
  return line.startsWith("diff --git ") || line.startsWith("Index: ") || line.startsWith("--- ")
}

function normalizeDeletedFilePatch(diff: string) {
  const lines = diff.split("\n")
  const out: string[] = []
  let inDeletedFile = false

  for (const line of lines) {
    if (line.startsWith("diff --git ")) {
      inDeletedFile = false
    } else if (line.startsWith("deleted file mode ")) {
      inDeletedFile = true
    } else if (line.startsWith("new file mode ")) {
      inDeletedFile = false
    }

    if (inDeletedFile && line.startsWith("+++ b/")) {
      const file = line.slice("+++ b/".length).trim()
      out.push(file ? `+++ /dev/null\t${file}` : "+++ /dev/null")
    } else {
      out.push(line)
    }
  }

  return out.join("\n")
}

function normalizeHunkCounts(diff: string) {
  const lines = diff.split("\n")
  const out: string[] = []

  for (let index = 0; index < lines.length;) {
    const line = lines[index]
    const header = parseHunkHeader(line)
    if (!header) {
      out.push(line)
      index += 1
      continue
    }

    const bodyStart = index + 1
    let bodyEnd = bodyStart
    while (bodyEnd < lines.length && !isPatchBoundary(lines[bodyEnd])) bodyEnd += 1

    const body = lines.slice(bodyStart, bodyEnd)
    const counts = countHunkBodyLines(body)

    out.push(formatHunkHeader(line, counts.oldCount, counts.newCount), ...body)
    index = bodyEnd
  }

  return out.join("\n")
}

type HunkHeader = {
  oldStart: string
  oldCount: number
  newStart: string
  newCount: number
}

function countHunkBodyLines(body: string[]) {
  let oldCount = 0
  let newCount = 0
  for (let index = 0; index < body.length; index += 1) {
    const line = body[index]
    const operation = line.length === 0 && index !== body.length - 1 ? " " : line[0]
    if (operation === " ") {
      oldCount += 1
      newCount += 1
    } else if (operation === "-") {
      oldCount += 1
    } else if (operation === "+") {
      newCount += 1
    }
  }
  return { oldCount, newCount }
}

function parseHunkHeader(line: string): HunkHeader | undefined {
  const match = line.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/)
  if (!match) return undefined
  return {
    oldStart: match[1],
    oldCount: match[2] === undefined ? 1 : Number(match[2]),
    newStart: match[3],
    newCount: match[4] === undefined ? 1 : Number(match[4]),
  }
}

function formatHunkHeader(original: string, oldCount: number, newCount: number) {
  return original.replace(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/, (_match, oldStart, _oldCount, newStart) => (
    `@@ -${formatRange(oldStart, oldCount)} +${formatRange(newStart, newCount)} @@`
  ))
}

function formatRange(start: string, count: number) {
  return count === 1 ? start : `${start},${count}`
}

function isPatchBoundary(line: string) {
  return line.startsWith("@@ ") || line.startsWith("diff --git ") || line.startsWith("--- ") || line.startsWith("Index: ")
}
