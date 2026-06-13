export type HunkReviewStatus = "pending" | "accepted" | "rejected"

export type ReviewHunk = {
  id: string
  header: string
  additions: number
  deletions: number
  preview: string
}

export type ReviewedHunk = ReviewHunk & {
  status: HunkReviewStatus
}

export function parseReviewHunks(diff: string): ReviewHunk[] {
  const lines = diff.split(/\r?\n/)
  const hunks: ReviewHunk[] = []
  let current: string[] = []
  let header = ""

  const flush = () => {
    if (!header) return
    const body = current.join("\n")
    hunks.push({
      id: `${hunks.length + 1}`,
      header,
      additions: current.filter((line) => line.startsWith("+") && !line.startsWith("+++")).length,
      deletions: current.filter((line) => line.startsWith("-") && !line.startsWith("---")).length,
      preview: body,
    })
  }

  for (const line of lines) {
    if (line.startsWith("@@")) {
      flush()
      header = line
      current = [line]
      continue
    }
    if (header) current.push(line)
  }
  flush()

  return hunks
}

export function createReviewedHunks(diff: string): ReviewedHunk[] {
  return parseReviewHunks(diff).map((hunk) => ({ ...hunk, status: "pending" }))
}

export function setHunkStatus(hunks: ReviewedHunk[], id: string, status: HunkReviewStatus): ReviewedHunk[] {
  return hunks.map((hunk) => (hunk.id === id ? { ...hunk, status } : hunk))
}

export function hunkReviewSummary(hunks: ReviewedHunk[]) {
  const accepted = hunks.filter((hunk) => hunk.status === "accepted").length
  const rejected = hunks.filter((hunk) => hunk.status === "rejected").length
  const pending = hunks.length - accepted - rejected
  return { total: hunks.length, accepted, rejected, pending }
}
