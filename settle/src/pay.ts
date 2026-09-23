// A run: one transfer per provider, then the receipts that say what
// it paid for. The order is the whole design — the receipts are how a
// later run knows not to pay again, so they follow the money and
// never lead it.

import { formatEther, type Hex } from "viem"
import type { Ledger, Owed } from "./ledger.ts"
import { receiptFor, type ReceiptRecord } from "./records.ts"
import { Unresolved } from "./sent.ts"

/** Sends GLM and answers with the transaction that carried it. */
export type Transfer = (to: Hex, amountWei: bigint) => Promise<Hex>

/** Writes receipts, as many as fit one Arkiv transaction. */
export type WriteReceipts = (records: ReceiptRecord[]) => Promise<void>

/**
 * How many receipts go in one transaction. Measured at about 72 under
 * the node's size cap; the margin is there because a receipt's size
 * follows its payload, and nothing stops a future field.
 */
const RECEIPTS_PER_BATCH = 50

export type Paid = {
  provider: Hex
  tx: Hex
  amountWei: bigint
  records: number
}

export type Run = {
  plan: Ledger
  /** The chain the transfers are made on, named in every receipt. */
  chainId: number
  transfer: Transfer
  write: WriteReceipts
  log: (line: string) => void
  /**
   * What stands in the way, from the balances a run read. Anything
   * here and it sends nothing: a run short of Arkiv gas would make
   * every transfer and write no receipt at all, which is the worst
   * state this can reach, and the payout chain's own refusals would
   * come too late to prevent it.
   */
  blockedBy: readonly string[]
}

/**
 * Pays what the ledger says. A provider is paid once, for every record
 * of theirs the ledger holds, and the receipts carry that transfer's
 * hash. A provider whose transfer fails is reported and the run goes
 * on to the next: what did not happen costs nothing, and the next run
 * finds the same records unpaid.
 */
export async function pay(run: Run): Promise<Paid[]> {
  if (run.blockedBy.length > 0) {
    throw new Error(`refusing to pay: ${run.blockedBy.join("; ")}`)
  }
  const paid: Paid[] = []
  for (const owed of run.plan.owed) {
    const receipt = await payOne(owed, run.chainId, run.transfer, run.write, run.log)
    if (receipt !== undefined) paid.push(receipt)
  }
  return paid
}

async function payOne(
  owed: Owed,
  chainId: number,
  transfer: Transfer,
  write: WriteReceipts,
  log: (line: string) => void,
): Promise<Paid | undefined> {
  let tx: Hex
  try {
    tx = await transfer(owed.provider, owed.amountWei)
  } catch (error) {
    // A transfer that never left costs nothing: the records stay
    // unpaid and the next run finds them. One that left and was not
    // seen to land is the other thing entirely — it may be mined
    // minutes later, and a run that called it a failure would send it
    // again.
    if (error instanceof Unresolved) {
      log(
        `${owed.provider}: MAY HAVE PAID. ${error.message}. Its ${owed.records.length} record${owed.records.length === 1 ? " has" : "s have"} no receipt`,
      )
      log(`${owed.provider}: do not run again before checking that transfer against the chain`)
    } else {
      log(`${owed.provider}: the transfer failed, and nothing was written: ${message(error)}`)
    }
    return undefined
  }
  log(
    `${owed.provider}: paid ${formatEther(owed.amountWei)} GLM for ${owed.records.length} record${owed.records.length === 1 ? "" : "s"}, ${tx}`,
  )

  // Every batch is tried, whatever the one before it did: they hold
  // different records, so one that lands is a record the next run
  // will not pay a second time, and a batch the node refuses for its
  // own reasons says nothing about the next.
  const receipts = owed.records.map((record) => receiptFor(record, { chainId, tx }))
  let missing = 0
  let refused: unknown
  for (let at = 0; at < receipts.length; at += RECEIPTS_PER_BATCH) {
    const batch = receipts.slice(at, at + RECEIPTS_PER_BATCH)
    try {
      await write(batch)
    } catch (error) {
      missing += batch.length
      refused = error
    }
  }
  if (missing > 0) {
    // The money is gone and these records still look unpaid, so the
    // next run would pay them again. Said as loudly as a line can, and
    // counting the records that have no receipt, which is what an
    // operator reconciles against the chain by hand.
    log(
      `${owed.provider}: PAID BUT NOT RECEIPTED. ${missing} record${missing === 1 ? "" : "s"} of transfer ${tx} have no receipt: ${message(refused)}`,
    )
    log(`${owed.provider}: do not run again before checking the transfer against the chain`)
    return undefined
  }
  return {
    provider: owed.provider,
    tx,
    amountWei: owed.amountWei,
    records: owed.records.length,
  }
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
