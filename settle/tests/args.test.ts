// What a run was asked to do. Run: npm test

import { deepStrictEqual as deepEqual, throws } from "node:assert/strict"
import { describe, it } from "node:test"
import { parseArgs } from "../src/args.ts"

describe("the command line", () => {
  it("rehearses when it is told nothing", () => {
    deepEqual(parseArgs([]), { paying: false, help: false })
  })

  it("pays when it is told to", () => {
    deepEqual(parseArgs(["--pay"]), { paying: true, help: false })
  })

  it("takes the word for what it does anyway", () => {
    deepEqual(parseArgs(["--rehearse"]), { paying: false, help: false })
  })

  it("refuses both at once", () => {
    throws(() => parseArgs(["--pay", "--rehearse"]), /different things/)
  })

  it("refuses what it does not understand", () => {
    // Silently ignoring a misspelt --pay would rehearse when a run
    // was meant to pay, and the operator would read "nothing was
    // written" as the ledger being empty.
    throws(() => parseArgs(["--pai"]), /unknown argument --pai/)
    throws(() => parseArgs(["--provider", "0x1"]), /unknown argument/)
  })

  it("asks for help", () => {
    deepEqual(parseArgs(["--help"]), { paying: false, help: true })
    deepEqual(parseArgs(["-h"]), { paying: false, help: true })
  })
})
