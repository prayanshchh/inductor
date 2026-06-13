import { execFile } from "node:child_process"
import { readFile, rm } from "node:fs/promises"
import { platform, release, tmpdir } from "node:os"
import path from "node:path"
import { promisify } from "node:util"

const exec = promisify(execFile)

export type ClipboardPayload =
  | { type: "image"; mime: "image/png"; base64: string }
  | { type: "text"; text: string }

export async function readClipboard(): Promise<ClipboardPayload | undefined> {
  const image = await readClipboardImage()
  if (image) return image

  const { default: clipboardy } = await import("clipboardy")
  const text = await clipboardy.read().catch(() => undefined)
  return text ? { type: "text", text } : undefined
}

async function readClipboardImage(): Promise<Extract<ClipboardPayload, { type: "image" }> | undefined> {
  if (platform() === "darwin") {
    const file = path.join(tmpdir(), `inductor-clipboard-${process.pid}.png`)
    try {
      await exec("osascript", [
        "-e",
        'set imageData to the clipboard as "PNGf"',
        "-e",
        `set fileRef to open for access POSIX file "${file}" with write permission`,
        "-e",
        "set eof fileRef to 0",
        "-e",
        "write imageData to fileRef",
        "-e",
        "close access fileRef",
      ])
      return { type: "image", mime: "image/png", base64: (await readFile(file)).toString("base64") }
    } catch {
      return undefined
    } finally {
      await rm(file, { force: true }).catch(() => undefined)
    }
  }

  if (platform() === "win32" || release().includes("WSL")) {
    const script = "Add-Type -AssemblyName System.Windows.Forms; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img) { $ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); [System.Convert]::ToBase64String($ms.ToArray()) }"
    const { stdout } = await exec("powershell.exe", ["-NonInteractive", "-NoProfile", "-command", script]).catch(() => ({ stdout: "" }))
    const base64 = stdout.toString().trim()
    return base64 ? { type: "image", mime: "image/png", base64 } : undefined
  }

  if (platform() === "linux") {
    const wayland = await exec("wl-paste", ["-t", "image/png"], { encoding: "buffer" }).catch(() => undefined)
    if (wayland?.stdout?.length) return { type: "image", mime: "image/png", base64: wayland.stdout.toString("base64") }
    const x11 = await exec("xclip", ["-selection", "clipboard", "-t", "image/png", "-o"], { encoding: "buffer" }).catch(() => undefined)
    if (x11?.stdout?.length) return { type: "image", mime: "image/png", base64: x11.stdout.toString("base64") }
  }

  return undefined
}