import { describe, expect, test } from "bun:test"
import { isTerminalMouseSequence, terminalModeResetSequence } from "../src/terminal_sequences"

describe("terminal control sequence filtering", () => {
  test("detects full SGR mouse sequences", () => {
    expect(isTerminalMouseSequence("\x1b[<65;129;29M")).toBe(true)
    expect(isTerminalMouseSequence("\x1b[<65;129;29m")).toBe(true)
  })

  test("detects leaked SGR mouse tails", () => {
    expect(isTerminalMouseSequence("<65;129;29M")).toBe(true)
    expect(isTerminalMouseSequence("<0;10;5m")).toBe(true)
  })

  test("does not treat normal text as mouse input", () => {
    expect(isTerminalMouseSequence("hello")).toBe(false)
    expect(isTerminalMouseSequence("<not;a;mouseM")).toBe(false)
    expect(isTerminalMouseSequence("x<65;129;29M")).toBe(false)
  })

  test("reset sequence disables mouse tracking modes", () => {
    expect(terminalModeResetSequence).toContain("\x1b[?1000l")
    expect(terminalModeResetSequence).toContain("\x1b[?1002l")
    expect(terminalModeResetSequence).toContain("\x1b[?1003l")
    expect(terminalModeResetSequence).toContain("\x1b[?1006l")
  })
})
