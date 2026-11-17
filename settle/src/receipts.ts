// Receipts for a transfer that landed without them: a run that paid
// and could not write its receipts, or lost sight of a transfer that
// was mined after all. Until they exist the records read as unpaid,
// and the next run would pay them again.

import { formatEther, isHash, type Hex } from "viem"
import { amountOf } from "./ledger.ts"
import type { Moved } from "./payout.ts"
import type { Counter } from "./records.ts"

export const RECEIPTS_USAGE = `receipts — writes the receipts for a transfer that already landed

  npm run receipts -- <tx> <record>...            check and print, write nothing
  SETTLE_PRIVATE_KEY=0x... npm run receipts -- <tx> <record>...   write them

<tx> is the transfer a run printed, and each <record> a counter record
it paid. The transfer must be mined, sent by the settle key, and have
moved exactly what the records are worth to their provider; none of
the records may have a receipt yet. Nothing is written otherwise.

Configuration comes from the same environment as settle.`

export type ReceiptsRun = { tx: Hex; records: Hex[] }

/** The transfer and the records, each a 32-byte hash, the records without repeats. */
export function parseReceiptsArgs(argv: readonly string[]): ReceiptsRun {
  const [tx, ...records] = argv
  if (tx === undefined || records.length === 0) {
    throw new Error(`a transfer and at least one record are needed\n\n${RECEIPTS_USAGE}`)
  }
  for (const value of [tx, ...records]) {
    if (!isHash(value)) throw new Error(`${value} is not a transaction hash or an entity key`)
  }
  const unique = [...new Set(records.map((record) => record.toLowerCase()))]
  if (unique.length !== records.length) throw new Error("a record is named twice")
  return { tx: tx as Hex, records: unique as Hex[] }
}

/** One named record as the chain holds it. */
export type Found = {
  recordKey: Hex
  /** The LB's closed counter record under that key, if there is one. */
  record?: Counter
  /** Whether a receipt of the settle key, or of an earlier one, names it. */
  receipted: boolean
}

export type Checked =
  | { ok: true; records: Counter[]; provider: Hex; amountWei: bigint }
  | { ok: false; problems: string[] }

/**
 * Whether these records were paid by this transfer and still lack
 * their receipts. Every problem is listed, not just the first, so one
 * read of the output says everything that is wrong.
 */
export function check(input: {
  tx: Hex
  moved: Moved | undefined
  settle: Hex
  found: Found[]
  /** What the receipts that already name this transfer account for, in wei. */
  receiptedWei: bigint
}): Checked {
  const problems: string[] = []
  const settle = input.settle.toLowerCase()

  // The transfer: mined, successful, sent by the settle key.
  const moved = input.moved
  if (moved === undefined) {
    problems.push(`the transfer ${input.tx} is not mined on the payout chain`)
  } else {
    if (moved.status !== "success") problems.push(`the transfer ${input.tx} reverted, and moved nothing`)
    if (moved.sender.toLowerCase() !== settle) {
      problems.push(`the transfer was sent by ${moved.sender}, not by the settle key ${input.settle}`)
    }
  }

  // The records: the LB's, closed, unpaid, all one provider's.
  const records: Counter[] = []
  for (const found of input.found) {
    if (found.record === undefined) {
      problems.push(`${found.recordKey} is not a closed counter record of the LB`)
      continue
    }
    if (found.receipted) problems.push(`${found.recordKey} already has a receipt`)
    records.push(found.record)
  }
  const providers = [...new Set(records.map((record) => record.provider.toLowerCase()))]
  if (providers.length > 1) problems.push(`the records belong to more than one provider: ${providers.join(", ")}`)

  // The amount: what the settle key sent the provider in this
  // transaction is what the records are worth, to the wei. A run pays
  // a provider's records in one transfer, so anything else is the
  // wrong transfer or the wrong records. Receipts that already name
  // the transfer take their share: they let a transfer whose receipts
  // were partly written be finished, and stop one whose receipts all
  // exist from paying for another record of the same worth.
  const provider = providers[0] as Hex | undefined
  const amountWei = records.reduce((sum, record) => sum + amountOf(record), 0n)
  if (moved !== undefined && provider !== undefined && providers.length === 1) {
    const sent = moved.transfers
      .filter((transfer) => transfer.from.toLowerCase() === settle && transfer.to.toLowerCase() === provider)
      .reduce((sum, transfer) => sum + transfer.amountWei, 0n)
    if (sent !== input.receiptedWei + amountWei) {
      const already =
        input.receiptedWei > 0n ? `, receipts already account for ${formatEther(input.receiptedWei)} of it` : ""
      problems.push(
        `the transfer sent ${formatEther(sent)} GLM to ${provider}${already}, and the records are worth ${formatEther(amountWei)}`,
      )
    }
  }

  if (problems.length > 0 || provider === undefined) return { ok: false, problems }
  return { ok: true, records, provider, amountWei }
}
