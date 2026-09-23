// Whose records a run pays, and whose receipts say a record is paid.

import { getAddress, type Hex } from "viem"

/**
 * The key gives it when a run has one, and it is configured for a
 * rehearsal that does not. Both and disagreeing is refused rather
 * than resolved: one of the two is a mistake, and picking either
 * would rehearse one ledger and pay another.
 */
export function settleAddress(fromKey?: Hex, configured?: string): Hex {
  const named = configured === undefined ? undefined : getAddress(configured)
  if (fromKey === undefined) {
    if (named === undefined) {
      throw new Error("no key and no SETTLE_ADDRESS: nothing says whose receipts to read")
    }
    return named
  }
  if (named !== undefined && named !== fromKey) {
    throw new Error(`the key is ${fromKey} and SETTLE_ADDRESS says ${named}`)
  }
  return fromKey
}
