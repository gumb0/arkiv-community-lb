// What a run owes: every closed counter record the LB wrote that no
// receipt of ours names yet, grouped by provider.

import type { Reader } from "./chain.ts"
import {
  KIND,
  attrAddr,
  attrStr,
  byKind,
  creator,
  decodeCounter,
  decodeReceipt,
  type Counter,
} from "./records.ts"
import type { Hex } from "viem"

/** What one provider is owed, and for which records. */
export type Owed = {
  provider: Hex
  records: Counter[]
  /** The sum of the records below, in wei of GLM. */
  amountWei: bigint
}

export type Ledger = {
  owed: Owed[]
  /** Records already named by a receipt, skipped. */
  paid: number
  /** The sum of what the providers above are owed, for the line a
   * run prints. Nothing already paid is in it. */
  totalOwedWei: bigint
}

/** Owed for one record: its count at the rate the record itself carries. */
export function amountOf(record: Counter): bigint {
  return record.count * record.weiPerCall
}

/**
 * The ledger a run would pay. Closed records are read once; a
 * provider's receipts are read per provider, which is one query per
 * provider that has anything owed and none for the rest.
 */
export async function ledger(reader: Reader, lb: Hex, settle: Hex): Promise<Ledger> {
  const closed = (
    await reader.query(byKind(KIND.counter, creator(lb), attrStr("state", "closed")))
  ).map(decodeCounter)

  const byProvider = new Map<Hex, Counter[]>()
  for (const record of closed) {
    const provider = record.provider.toLowerCase() as Hex
    const records = byProvider.get(provider)
    if (records === undefined) {
      byProvider.set(provider, [record])
    } else {
      records.push(record)
    }
  }

  const owed: Owed[] = []
  let paid = 0
  for (const [provider, records] of byProvider) {
    const receipts = (
      await reader.query(byKind(KIND.receipt, creator(settle), attrAddr("provider", provider)))
    ).map(decodeReceipt)
    const settled = new Set(receipts.map((receipt) => receipt.counter.toLowerCase()))
    const unpaid = records.filter((record) => !settled.has(record.key.toLowerCase()))
    paid += records.length - unpaid.length
    if (unpaid.length === 0) continue
    // Oldest period first, so a run that is interrupted has paid the
    // records a provider has been waiting longest for.
    unpaid.sort((left, right) => Number(left.closedBlock - right.closedBlock))
    owed.push({
      provider,
      records: unpaid,
      amountWei: unpaid.reduce((sum, record) => sum + amountOf(record), 0n),
    })
  }
  owed.sort((left, right) => (left.provider < right.provider ? -1 : 1))

  return {
    owed,
    paid,
    totalOwedWei: owed.reduce((sum, entry) => sum + entry.amountWei, 0n),
  }
}
