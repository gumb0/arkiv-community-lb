// Receipts for a transfer that landed without them. Run: npm test

import { deepStrictEqual as deepEqual, strictEqual as equal, throws } from "node:assert/strict"
import { describe, it } from "node:test"
import type { Hex } from "viem"
import type { Moved } from "../src/payout.ts"
import { check as checkWith, parseReceiptsArgs, type Found } from "../src/receipts.ts"
import { decodeCounter } from "../src/records.ts"
import { address, closedCounter, key, SETTLE } from "./chain.ts"

const TX = key(0xff)
const PROVIDER = address(3)
const MILLI = 10n ** 15n

/** A record of PROVIDER's worth `count` requests at 0.001 GLM, as found on the chain. */
function found(n: number, count: number, options: { receipted?: boolean; provider?: Hex } = {}): Found {
  return {
    recordKey: key(n),
    record: decodeCounter(closedCounter({ key: key(n), provider: options.provider ?? PROVIDER, count })),
    receipted: options.receipted ?? false,
  }
}

/** A mined transfer from the settle key to PROVIDER. */
function moved(amountWei: bigint, options: Partial<Moved> = {}): Moved {
  return {
    status: "success",
    sender: SETTLE,
    transfers: [{ from: SETTLE, to: PROVIDER, amountWei }],
    ...options,
  }
}

/** The check, with no receipt naming the transfer unless a test says so. */
function check(
  input: Omit<Parameters<typeof checkWith>[0], "receiptedWei"> & { receiptedWei?: bigint },
): ReturnType<typeof checkWith> {
  return checkWith({ receiptedWei: 0n, ...input })
}

function problems(result: ReturnType<typeof check>): string[] {
  return result.ok ? [] : result.problems
}

describe("receipts for a transfer that landed", () => {
  it("accepts a transfer that paid exactly the records' worth", () => {
    const result = check({ tx: TX, moved: moved(2244n * MILLI), settle: SETTLE, found: [found(1, 2244)] })
    equal(result.ok, true)
    if (result.ok) {
      equal(result.amountWei, 2244n * MILLI)
      equal(result.provider, PROVIDER.toLowerCase())
    }
  })

  it("accepts one transfer for several records of one provider", () => {
    // A run pays all of a provider's records in one transfer.
    const result = check({
      tx: TX,
      moved: moved(30n * MILLI),
      settle: SETTLE,
      found: [found(1, 10), found(2, 20)],
    })
    equal(result.ok, true)
  })

  it("refuses a transfer that is not mined", () => {
    deepEqual(problems(check({ tx: TX, moved: undefined, settle: SETTLE, found: [found(1, 1)] })), [
      `the transfer ${TX} is not mined on the payout chain`,
    ])
  })

  it("refuses a transfer that reverted", () => {
    const result = check({
      tx: TX,
      moved: moved(MILLI, { status: "reverted", transfers: [] }),
      settle: SETTLE,
      found: [found(1, 1)],
    })
    equal(problems(result)[0], `the transfer ${TX} reverted, and moved nothing`)
  })

  it("refuses a transfer the settle key did not send", () => {
    const other = address(9)
    const result = check({
      tx: TX,
      moved: moved(MILLI, { sender: other, transfers: [{ from: other, to: PROVIDER, amountWei: MILLI }] }),
      settle: SETTLE,
      found: [found(1, 1)],
    })
    // The amount check counts only what the settle key sent, so both say so.
    equal(problems(result).length, 2)
    equal(problems(result)[0], `the transfer was sent by ${other}, not by the settle key ${SETTLE}`)
  })

  it("refuses a key that is not a closed counter record of the LB", () => {
    const result = check({
      tx: TX,
      moved: moved(MILLI),
      settle: SETTLE,
      found: [found(1, 1), { recordKey: key(2), receipted: false }],
    })
    deepEqual(problems(result), [`${key(2)} is not a closed counter record of the LB`])
  })

  it("refuses a record that already has a receipt", () => {
    // Writing a second receipt changes nothing settle reads, but it
    // says the record was paid twice.
    deepEqual(
      problems(check({ tx: TX, moved: moved(MILLI), settle: SETTLE, found: [found(1, 1, { receipted: true })] })),
      [`${key(1)} already has a receipt`],
    )
  })

  it("refuses records of more than one provider", () => {
    const result = check({
      tx: TX,
      moved: moved(2n * MILLI),
      settle: SETTLE,
      found: [found(1, 1), found(2, 1, { provider: address(4) })],
    })
    equal(problems(result).length, 1)
    equal(problems(result)[0]?.startsWith("the records belong to more than one provider"), true)
  })

  it("refuses an amount that is not the records' worth", () => {
    deepEqual(problems(check({ tx: TX, moved: moved(1000n * MILLI), settle: SETTLE, found: [found(1, 2244)] })), [
      `the transfer sent 1 GLM to ${PROVIDER.toLowerCase()}, and the records are worth 2.244`,
    ])
  })

  it("refuses a transfer whose receipts all exist, for another record of the same worth", () => {
    // The transfer paid one record, which has its receipt. A second
    // unpaid record worth the same must not be marked paid by it.
    deepEqual(
      problems(
        check({ tx: TX, moved: moved(2244n * MILLI), settle: SETTLE, found: [found(2, 2244)], receiptedWei: 2244n * MILLI }),
      ),
      [
        `the transfer sent 2.244 GLM to ${PROVIDER.toLowerCase()}, receipts already account for 2.244 of it, and the records are worth 2.244`,
      ],
    )
  })

  it("finishes a transfer whose receipts were partly written", () => {
    // Paid 30 for two records; the receipt for the first landed.
    const result = check({
      tx: TX,
      moved: moved(30n * MILLI),
      settle: SETTLE,
      found: [found(2, 20)],
      receiptedWei: 10n * MILLI,
    })
    equal(result.ok, true)
  })

  it("refuses a transfer to someone else", () => {
    const result = check({
      tx: TX,
      moved: moved(MILLI, { transfers: [{ from: SETTLE, to: address(4), amountWei: MILLI }] }),
      settle: SETTLE,
      found: [found(1, 1)],
    })
    equal(problems(result)[0], `the transfer sent 0 GLM to ${PROVIDER.toLowerCase()}, and the records are worth 0.001`)
  })
})

describe("the receipts command line", () => {
  it("takes a transfer and its records", () => {
    deepEqual(parseReceiptsArgs([TX, key(1), key(2)]), { tx: TX, records: [key(1), key(2)] })
  })

  it("needs at least one record", () => {
    throws(() => parseReceiptsArgs([TX]), /a transfer and at least one record are needed/)
  })

  it("refuses what is not a 32-byte hash", () => {
    throws(() => parseReceiptsArgs([TX, "0x1234"]), /0x1234 is not a transaction hash or an entity key/)
  })

  it("refuses a record named twice", () => {
    throws(() => parseReceiptsArgs([TX, key(1), key(1)]), /a record is named twice/)
  })
})
