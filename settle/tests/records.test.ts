// The records: what settle reads from a counter record and from its
// own receipts, and how it builds one. Run: npm test

import { deepStrictEqual as deepEqual, strictEqual as equal, throws } from "node:assert/strict"
import { describe, it } from "node:test"
import { ExpirationTime } from "@arkiv-network/sdk"
import { stringToBytes } from "viem"
import { decodeCounter, decodeReceipt, receiptFor } from "../src/records.ts"
import { closedCounter, key, provider, receipt } from "./chain.ts"

describe("a closed counter record", () => {
  it("reads its count, its own rate and the blocks it covers", () => {
    const record = decodeCounter(
      closedCounter({
        key: key(7),
        provider: provider(1),
        agreement: key(0xaa),
        count: 48213,
        weiPerCall: 10n ** 15n,
        openedBlock: 1204000,
        closedBlock: 1506400,
      }),
    )
    equal(record.key, key(7))
    equal(record.agreement, key(0xaa))
    equal(record.provider, provider(1))
    equal(record.count, 48213n)
    equal(record.weiPerCall, 10n ** 15n)
    equal(record.openedBlock, 1204000n)
    equal(record.closedBlock, 1506400n)
  })

  it("refuses one that names no closing block", () => {
    // Only a closed record is payable, and a closed record has one.
    const open = closedCounter({ key: key(7), provider: provider(1), count: 1 })
    open.payload = stringToBytes(
      JSON.stringify({ count: 1, wei_per_call: "1000", opened_block: 10 }),
    )
    throws(() => decodeCounter(open), /closing block/)
  })

  it("refuses one whose attributes are missing", () => {
    const record = closedCounter({ key: key(7), provider: provider(1), count: 1 })
    delete (record.attributes as Record<string, unknown>).provider
    throws(() => decodeCounter(record), /addr attribute provider/)
  })
})

describe("a receipt", () => {
  it("reads which record it paid and what the transfer was", () => {
    const paid = decodeReceipt(
      receipt({ key: key(9), counter: key(7), provider: provider(1), amountWei: 5n * 10n ** 18n }),
    )
    equal(paid.counter, key(7))
    equal(paid.provider, provider(1))
    equal(paid.amountWei, 5n * 10n ** 18n)
    equal(paid.payout.chainId, 560048)
  })

  it("is written permanent and readonly", () => {
    // Settle reads a counter record as unpaid until one of its own
    // receipts names it, so a receipt that expired before its record
    // would have the next run pay for it again. Readonly because the
    // evidence of a payment must not be editable, by settle either.
    const record = receiptFor(
      decodeCounter(closedCounter({ key: key(7), provider: provider(1), count: 10 })),
      { chainId: 560048, tx: key(0xff) },
    )
    deepEqual(record.expires, ExpirationTime.permanent())
    equal(record.flags.readonly, true)
  })
})
