import { existsSync, readdirSync, statSync } from "node:fs"
import path from "node:path"

export type MentionState = {
  triggerStart: number
  token: string
  dir: string
  query: string
}

export type FileChoice = {
  name: string
  path: string
  kind: "file" | "dir"
  description: string
}

export type PromptImageAttachment = { label: string; path: string }

const ignoredDirs = new Set([".git", "node_modules", "target"])
const imageExtensions = new Set([".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tif", ".tiff"])

export function findActiveMention(value: string): MentionState | undefined {
  const triggerStart = value.lastIndexOf("@")
  if (triggerStart < 0) return undefined
  if (triggerStart > 0 && !/\s/.test(value[triggerStart - 1])) return undefined

  const token = value.slice(triggerStart)
  if (/\s/.test(token)) return undefined

  const raw = token.slice(1)
  const slash = raw.lastIndexOf("/")
  const dir = slash >= 0 ? raw.slice(0, slash + 1) : ""
  const query = slash >= 0 ? raw.slice(slash + 1) : raw
  return { triggerStart, token, dir, query }
}

export function listFileChoices(workspace: string, mention: MentionState, limit = 12): FileChoice[] {
  const dirPath = safeJoin(workspace, mention.dir)
  if (!dirPath) return []

  let entries: string[]
  try {
    entries = readdirSync(dirPath)
  } catch {
    return []
  }

  const query = mention.query.toLowerCase()
  return entries
    .filter((entry) => entry.toLowerCase().includes(query))
    .flatMap((entry): FileChoice[] => {
      const absolute = path.join(dirPath, entry)
      let stat
      try {
        stat = statSync(absolute)
      } catch {
        return []
      }
      const isDir = stat.isDirectory()
      if (isDir && ignoredDirs.has(entry)) return []
      if (!isDir && !stat.isFile()) return []
      const relative = toWorkspacePath(path.join(mention.dir, entry))
      return [{
        name: isDir ? `${entry}/` : entry,
        path: isDir ? `${relative}/` : relative,
        kind: isDir ? "dir" : "file",
        description: isDir ? "directory" : fileDescription(entry, stat.size),
      }]
    })
    .sort((a, b) => Number(b.kind === "dir") - Number(a.kind === "dir") || a.name.localeCompare(b.name))
    .slice(0, limit)
}

export function replaceMention(value: string, mention: MentionState, choice: FileChoice, insertDirectory = false) {
  const suffix = choice.kind === "dir" && !insertDirectory ? choice.path : `${choice.path} `
  return `${value.slice(0, mention.triggerStart)}@${suffix}`
}

export function appendPromptToken(value: string, token: string) {
  const prefix = value.trimEnd()
  return `${prefix}${prefix ? " " : ""}${token} `
}

export function isImagePath(value: string) {
  return imageExtensions.has(path.extname(stripToken(value)).toLowerCase())
}

export function pastedImageName(index: number, extension = ".png") {
  return `pasted-image-${index}${extension}`
}

export function promptForSubmit(value: string, images: PromptImageAttachment[]) {
  if (images.length === 0) return value
  let prompt = value
  for (const image of images) {
    prompt = prompt.replace(image.label, `${image.label} @${image.path}`)
  }
  return prompt
}

export function stripToken(value: string) {
  return value
    .trim()
    .replace(/^file:\/\//, "")
    .replace(/[),.;:]+$/g, "")
    .replace(/^['\"]|['\"]$/g, "")
    .replace(/\\ /g, " ")
}

export function toWorkspacePath(value: string) {
  return value.split(path.sep).join("/").replace(/^\.\//, "")
}

function safeJoin(workspace: string, relative: string) {
  const resolved = path.resolve(workspace, relative || ".")
  const root = path.resolve(workspace)
  if (resolved !== root && !resolved.startsWith(`${root}${path.sep}`)) return undefined
  return resolved
}

function fileDescription(name: string, size: number) {
  return `${path.extname(name).replace(/^\./, "") || "file"} · ${formatBytes(size)}`
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}