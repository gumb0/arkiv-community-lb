// A transfer that was broadcast and then lost sight of. Everything a
// run does after sending turns on this distinction: a transfer that
// never left is a failure costing nothing, and one that left and was
// not seen to land may have moved money, which only a person can
// settle.

import type { Hex } from "viem"

/** Thrown when the transaction is on the wire and its outcome is not. */
export class Unresolved extends Error {
  tx: Hex

  constructor(tx: Hex, reason: unknown) {
    super(`the transfer ${tx} was sent and its outcome is unknown: ${text(reason)}`)
    this.name = "Unresolved"
    this.tx = tx
  }
}

function text(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
