// What a run would pay, from the records alone. Run: npm test

import { deepStrictEqual as deepEqual, strictEqual as equal } from "node:assert/strict"
import { describe, it } from "node:test"
import { amountOf, ledger } from "../src/ledger.ts"
import { LB, SETTLE, closedCounter, fakeChain, key, provider, receipt } from "./chain.ts"

const GLM = 10n ** 18n

describe("the ledger", () => {
  it("owes each provider its records at the rate each record carries", async () => {
    // The rate is the record's own: the LB's configuration may have
    // moved since, and the deal it was counted under did not.
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 1000, weiPerCall: GLM / 1000n }),
      closedCounter({ key: key(2), provider: provider(1), count: 500, weiPerCall: GLM / 500n }),
      closedCounter({ key: key(3), provider: provider(2), count: 1, weiPerCall: GLM }),
    ])

    const plan = await ledger(chain, LB, SETTLE)
    equal(plan.owed.length, 2)
    const [first, second] = plan.owed
    equal(first?.provider, provider(1).toLowerCase())
    equal(first?.amountWei, 2n * GLM, "one GLM from each of its records")
    equal(second?.amountWei, GLM)
    equal(plan.totalOwedWei, 3n * GLM)
    equal(plan.paid, 0)
  })

  it("skips a record a receipt of ours already names", async () => {
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      closedCounter({ key: key(2), provider: provider(1), count: 20 }),
      receipt({ key: key(0x11), counter: key(1), provider: provider(1) }),
    ])

    const plan = await ledger(chain, LB, SETTLE)
    equal(plan.paid, 1)
    deepEqual(
      plan.owed[0]?.records.map((record) => record.key),
      [key(2)],
    )
  })

  it("pays nobody when every record is receipted", async () => {
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      receipt({ key: key(0x11), counter: key(1), provider: provider(1) }),
    ])

    const plan = await ledger(chain, LB, SETTLE)
    deepEqual(plan.owed, [])
    equal(plan.totalOwedWei, 0n)
    equal(plan.paid, 1)
  })

  it("counts a receipt written by another key for nothing", async () => {
    // Only settle's own receipts say a record is paid; anyone can
    // write a record that looks like one.
    const stranger = provider(9)
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10 }),
      receipt({ key: key(0x11), counter: key(1), provider: provider(1), creator: stranger }),
    ])

    const plan = await ledger(chain, LB, SETTLE)
    equal(plan.paid, 0)
    equal(plan.owed[0]?.records.length, 1)
  })

  it("leaves another LB's records alone", async () => {
    const other = provider(8)
    const chain = fakeChain([
      closedCounter({ key: key(1), provider: provider(1), count: 10, creator: other }),
    ])

    const plan = await ledger(chain, LB, SETTLE)
    deepEqual(plan.owed, [])
  })

  it("puts a provider's oldest period first", async () => {
    const chain = fakeChain([
      closedCounter({ key: key(2), provider: provider(1), count: 1, closedBlock: 900 }),
      closedCounter({ key: key(1), provider: provider(1), count: 1, closedBlock: 300 }),
      closedCounter({ key: key(3), provider: provider(1), count: 1, closedBlock: 600 }),
    ])

    const plan = await ledger(chain, LB, SETTLE)
    deepEqual(
      plan.owed[0]?.records.map((record) => record.closedBlock),
      [300n, 600n, 900n],
    )
  })

  it("reads the receipts of a provider that has records, and of no other", async () => {
    const chain = fakeChain([closedCounter({ key: key(1), provider: provider(1), count: 10 })])

    await ledger(chain, LB, SETTLE)
    const receiptReads = chain.asked.filter((text) => text.includes("rpc.receipt"))
    equal(receiptReads.length, 1, "one provider, one read")
  })
})

describe("what one record is owed", () => {
  it("is its count at its own rate, in wei", () => {
    equal(
      amountOf({
        key: key(1),
        agreement: key(0xaa),
        provider: provider(1),
        count: 48213n,
        weiPerCall: 10n ** 15n,
        openedBlock: 1204000n,
        closedBlock: 1506400n,
      }),
      48213n * 10n ** 15n,
    )
  })
})
