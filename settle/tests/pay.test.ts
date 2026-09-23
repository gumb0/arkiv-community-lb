// A run: the transfer, then the receipts that record it. Run: npm test

import { deepStrictEqual as deepEqual, strictEqual as equal, rejects } from "node:assert/strict"
import { describe, it } from "node:test"
import { bytesToString, type Hex } from "viem"
import { ledger, type Ledger } from "../src/ledger.ts"
import { pay } from "../src/pay.ts"
import { Unresolved } from "../src/sent.ts"
import type { ReceiptRecord } from "../src/records.ts"
import {
  LB,
  SETTLE,
  closedCounter,
  fakeChain,
  key,
  provider,
  receipt,
  written,
} from "./chain.ts"

const CHAIN = 560048
const TX = `0x${"ab".repeat(32)}` as Hex

/**
 * A run over fakes: what it sent, what it wrote and what it said.
 * `failFor` refuses the transfer to one provider, `failWrites`
 * refuses every receipt write.
 */
function run(
  options: {
    failFor?: Hex
    failWrites?: boolean
    refuseBatch?: number
    /** The transfer is sent and never seen to land. */
    unresolved?: boolean
  } = {},
) {
  const sent: { to: Hex; amountWei: bigint }[] = []
  const writes: ReceiptRecord[][] = []
  let attempts = 0
  const lines: string[] = []
  return {
    sent,
    writes,
    lines,
    /** Every receipt written, across however many batches. */
    get written(): ReceiptRecord[] {
      return writes.flat()
    },
    pay: (plan: Ledger, blockedBy: readonly string[] = []) =>
      pay({
        plan,
        chainId: CHAIN,
        blockedBy,
        transfer: async (to, amountWei) => {
          if (to.toLowerCase() === options.failFor?.toLowerCase()) {
            throw new Error("insufficient funds")
          }
          sent.push({ to, amountWei })
          if (options.unresolved === true) throw new Unresolved(TX, new Error("timed out"))
          return TX
        },
        write: async (records) => {
          attempts += 1
          if (options.failWrites === true) throw new Error("the sidecar is down")
          if (attempts === options.refuseBatch) throw new Error("the node refused this one")
          writes.push(records)
        },
        log: (line) => lines.push(line),
      }),
  }
}

function payloadOf(record: ReceiptRecord): Record<string, unknown> {
  return JSON.parse(bytesToString(record.payload)) as Record<string, unknown>
}

describe("a run", () => {
  it("sends one transfer per provider and a receipt per record", async () => {
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      closedCounter({ key: key(2), provider: provider(1), count: 20 }),
      closedCounter({ key: key(3), provider: provider(2), count: 5 }),
    ])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run()

    const paid = await it.pay(plan)

    equal(it.sent.length, 2, "one transfer each, not one per record")
    equal(it.written.length, 3, "one receipt each, not one per provider")
    equal(paid.length, 2)
    equal(
      it.sent.find((transfer) => transfer.to === provider(1).toLowerCase())?.amountWei,
      30n * 10n ** 15n,
      "both records in one transfer",
    )
  })

  it("writes what the record was paid for, and by which transfer", async () => {
    const chain = fakeChain([
      closedCounter({
        key: key(1),
        provider: provider(1),
        agreement: key(0xaa),
        count: 48213,
        weiPerCall: 10n ** 15n,
      }),
    ])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run()

    await it.pay(plan)

    const [record] = it.written
    const payload = payloadOf(record as ReceiptRecord)
    equal(payload.agreement, key(0xaa))
    equal(payload.count, 48213)
    equal(payload.wei_per_call, "1000000000000000")
    equal(payload.amount_wei, (48213n * 10n ** 15n).toString(), "the count at the record's rate")
    deepEqual(payload.payout, { chain_id: CHAIN, tx: TX })
  })

  it("pays nobody the second time", async () => {
    const rows = [closedCounter({ key: key(1), provider: provider(1), count: 10 })]
    const first = run()
    const paidFirst = await first.pay(await ledger(fakeChain(rows), LB, SETTLE))
    equal(paidFirst.length, 1)
    equal(first.sent.length, 1)

    // The receipt the first run wrote is what the second one reads.
    rows.push(receipt({ key: key(0x11), counter: key(1), provider: provider(1) }))
    const again = run()
    const paid = await again.pay(await ledger(fakeChain(rows), LB, SETTLE))

    deepEqual(paid, [])
    equal(again.sent.length, 0, "nothing was sent")
  })

  it("writes receipts a later run reads as paid", async () => {
    // The two halves meet here: what pay writes is what ledger reads,
    // and the receipt's counter attribute is the whole of it.
    const rows = [
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      closedCounter({ key: key(2), provider: provider(1), count: 20 }),
      closedCounter({ key: key(3), provider: provider(2), count: 5 }),
    ]
    const first = run()
    await first.pay(await ledger(fakeChain(rows), LB, SETTLE))

    rows.push(...first.written.map((record, at) => written(record, key(0x100 + at))))
    const again = run()
    const plan = await ledger(fakeChain(rows), LB, SETTLE)
    const paid = await again.pay(plan)

    equal(plan.paid, 3, "every record of the first run is read as paid")
    deepEqual(paid, [])
    equal(again.sent.length, 0)
  })

  it("writes a provider's receipts in as few transactions as it can", async () => {
    // A hundred receipts are not a hundred transactions: that is what
    // the batch is for.
    const rows = Array.from({ length: 120 }, (_, at) =>
      closedCounter({ key: key(at + 1), provider: provider(1), count: 1 }),
    )
    const it = run()

    await it.pay(await ledger(fakeChain(rows), LB, SETTLE))

    equal(it.written.length, 120, "one receipt each")
    equal(it.writes.length, 3, "50, 50 and 20")
    deepEqual(
      it.writes.map((batch) => batch.length),
      [50, 50, 20],
    )
  })

  it("sends nothing at all when something stands in the way", async () => {
    // Short of Arkiv gas, a run would make every transfer and write
    // no receipt: every provider paid with nothing to say so.
    const chain = fakeChain([closedCounter({ key: key(1), provider: provider(1), count: 10 })])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run()

    await rejects(
      () => it.pay(plan, ["no gas on Arkiv, where the receipts are written"]),
      /refusing to pay: no gas on Arkiv/,
    )
    equal(it.sent.length, 0)
    equal(it.written.length, 0)
  })

  it("tries every batch and counts the records left without a receipt", async () => {
    // A batch that fails says nothing about the next: the ones after
    // it still land, and each one that does is a record the next run
    // will not pay twice.
    const rows = Array.from({ length: 120 }, (_, at) =>
      closedCounter({ key: key(at + 1), provider: provider(1), count: 1 }),
    )
    const it = run({ refuseBatch: 2 })

    await it.pay(await ledger(fakeChain(rows), LB, SETTLE))

    equal(it.writes.length, 2, "the first and the third landed")
    equal(it.written.length, 70, "50 and 20")
    equal(
      it.lines.some((line) => line.includes("PAID BUT NOT RECEIPTED. 50 records")),
      true,
      it.lines.join("\n"),
    )
  })

  it("stops when a provider was paid and its receipts were not written", async () => {
    // What makes a receipt write fail is the same for the provider
    // after this one: no gas on Arkiv, the endpoint gone. Going on
    // would pay each of them and record none of it.
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      closedCounter({ key: key(2), provider: provider(2), count: 20 }),
      closedCounter({ key: key(3), provider: provider(3), count: 30 }),
    ])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run({ failWrites: true })

    const paid = await it.pay(plan)

    equal(paid.length, 1, "the one whose transfer landed")
    equal(paid[0]?.receipted, false)
    equal(it.sent.length, 1, "only the first provider was paid")
    equal(
      it.lines.some((line) => line.includes("stopping here: 2 providers were not paid at all")),
      true,
      it.lines.join("\n"),
    )
  })

  it("stops when it lost sight of a transfer", async () => {
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      closedCounter({ key: key(2), provider: provider(2), count: 20 }),
    ])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run({ unresolved: true })

    await it.pay(plan)

    equal(it.sent.length, 1, "the second provider is left alone")
  })

  it("stops when a transfer fails, and sends nothing after it", async () => {
    // Nothing that stops one transfer is about that provider: no gas,
    // an endpoint gone, a fee estimate that failed. Trying the next
    // one repeats it at best, and at worst sends a transfer into the
    // same conditions and loses sight of it.
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      closedCounter({ key: key(2), provider: provider(2), count: 20 }),
    ])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run({ failFor: provider(1) })

    const paid = await it.pay(plan)

    deepEqual(paid, [])
    equal(it.sent.length, 0, "nothing left the wallet")
    equal(it.written.length, 0)
    equal(
      it.lines.some((line) => line.includes("the transfer failed, and nothing was written")),
      true,
    )
    equal(
      it.lines.some((line) => line.includes("stopping here: 1 provider was not paid at all")),
      true,
      it.lines.join("\n"),
    )
  })

  it("says a transfer it lost sight of may have paid", async () => {
    // Viem gives up waiting after a few minutes of a chain that is
    // merely slow, and the transaction can be mined after that. A run
    // that called this a failure would send it again.
    const chain = fakeChain([closedCounter({ key: key(1), provider: provider(1), count: 10 })])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run({ unresolved: true })

    const paid = await it.pay(plan)

    deepEqual(paid, [], "not reported as paid")
    equal(it.written.length, 0, "and no receipt for a transfer nobody saw land")
    equal(
      it.lines.some((line) => line.includes("MAY HAVE PAID") && line.includes(TX)),
      true,
      it.lines.join("\n"),
    )
    equal(
      it.lines.some((line) => line.includes("do not run again before checking")),
      true,
    )
    equal(
      it.lines.some((line) => line.includes("the transfer failed")),
      false,
      "never called a failure",
    )
  })

  it("says so loudly when it paid and could not write the receipts", async () => {
    // The money is gone and the record still looks unpaid, which is
    // the one failure a later run cannot tell from an unpaid record.
    const chain = fakeChain([closedCounter({ key: key(1), provider: provider(1), count: 10 })])
    const plan = await ledger(chain, LB, SETTLE)
    const it = run({ failWrites: true })

    const paid = await it.pay(plan)

    equal(it.sent.length, 1, "the transfer went")
    equal(paid.length, 1, "and the money it moved is counted")
    equal(paid[0]?.receipted, false, "with nothing on the chain to say so")
    equal(
      it.lines.some((line) => line.includes("PAID BUT NOT RECEIPTED")),
      true,
    )
    equal(
      it.lines.some((line) => line.includes("do not run again before checking")),
      true,
    )
  })
})
