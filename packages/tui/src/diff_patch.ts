import { createTwoFilesPatch } from "diff"

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
