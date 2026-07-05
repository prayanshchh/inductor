const sgrMouseEvent = /\x1b\[<\d+;\d+;\d+[mM]/g
const leakedSgrMouseEvent = /<\d+;\d+;\d+[mM]/g

export const terminalModeResetSequence = [
  "\x1b[?1000l", // normal mouse tracking
  "\x1b[?1002l", // button-event mouse tracking
  "\x1b[?1003l", // any-event mouse tracking
  "\x1b[?1006l", // SGR extended mouse mode
  "\x1b[?2004l", // bracketed paste
  "\x1b[?25h", // show cursor
  "\x1b[0m", // reset style
].join("")

export function isTerminalMouseSequence(sequence: string | undefined): boolean {
  if (!sequence) return false
  return consumesEntireSequence(sequence, sgrMouseEvent) || consumesEntireSequence(sequence, leakedSgrMouseEvent)
}

function consumesEntireSequence(sequence: string, pattern: RegExp): boolean {
  pattern.lastIndex = 0
  let consumed = 0
  let match: RegExpExecArray | null
  while ((match = pattern.exec(sequence))) {
    if (match.index !== consumed) return false
    consumed = pattern.lastIndex
  }
  return consumed === sequence.length
}
