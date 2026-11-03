// Reading a query's pages to the end. Run: npm test

import { deepStrictEqual as deepEqual, rejects, strictEqual as equal } from "node:assert/strict"
import { describe, it } from "node:test"
import type { Hex } from "viem"
import { everyPage, PAGE, type Row } from "../src/chain.ts"
import { address, closedCounter, key } from "./chain.ts"

const row = (n: number) => closedCounter({ key: key(n), provider: address(1), count: n })
const range = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => from + i)

/** A node over a set of rows: a page is the rows after a key, in key order. */
function node(numbers: number[]) {
  const live = new Set(numbers)
  const asked: (Hex | undefined)[] = []
  const read = async (after: Hex | undefined): Promise<Row[]> => {
    asked.push(after)
    return [...live]
      .sort((a, b) => a - b)
      .filter((n) => after === undefined || BigInt(key(n)) > BigInt(after))
      .slice(0, PAGE)
      .map(row)
  }
  return { live, asked, read }
}

describe("reading every page", () => {
  it("reads the rows after the last key until an empty page", async () => {
    const chain = node(range(1, 250))

    const rows = await everyPage(chain.read)

    deepEqual(
      rows.map((r) => r.key),
      range(1, 250).map(key),
    )
    deepEqual(chain.asked, [undefined, key(100), key(200), key(250)])
  })

  it("reads on past a page the node cut short", async () => {
    // A node that bounded a page by size, say, would answer short pages
    // before the end; a walk that stopped at one would hide the rest.
    const chain = node(range(1, 50))
    const read = async (after: Hex | undefined) => (await chain.read(after)).slice(0, 20)

    const rows = await everyPage(read)

    deepEqual(
      rows.map((r) => r.key),
      range(1, 50).map(key),
    )
    deepEqual(chain.asked, [undefined, key(20), key(40), key(50)])
  })

  it("asks once for an empty answer", async () => {
    const chain = node([])

    deepEqual(await everyPage(chain.read), [])
    equal(chain.asked.length, 1)
  })

  it("keeps every live row when rows expire between pages", async () => {
    // Every fifth row expires once the first page is read, on both sides
    // of the last key read. By offset, the rows after it would shift and
    // as many would be skipped.
    const chain = node(range(1, 250))
    const read = async (after: Hex | undefined) => {
      const page = await chain.read(after)
      if (after === undefined) for (const n of range(1, 250)) if (n % 5 === 0) chain.live.delete(n)
      return page
    }

    const rows = await everyPage(read)

    deepEqual(
      rows.slice(0, PAGE).map((r) => r.key),
      range(1, PAGE).map(key),
      "the first page was read whole, before anything expired",
    )
    equal(chain.asked[1], key(PAGE), "the second page asked from the first page's last key")
    const got = new Set(rows.map((r) => r.key))
    for (const n of chain.live) equal(got.has(key(n)), true, `live row ${n} was read`)
    equal(got.size, rows.length, "no row twice")
  })

  it("refuses a page that does not move past the last key", async () => {
    // A node that ignored the bound would hand back the same page for ever.
    let reads = 0
    await rejects(
      everyPage(async () => {
        reads += 1
        return range(1, PAGE).map(row)
      }),
      /the page after key 0x0+64 out of key order: nothing was paid/,
    )
    equal(reads, 2)
  })

  it("refuses a page out of key order", async () => {
    // A node that ignored the order: the page's last key would be the
    // next bound, and the rows between would never be read.
    let reads = 0
    await rejects(
      everyPage(async () => {
        reads += 1
        return [2, 1, ...range(3, PAGE)].map(row)
      }),
      /the first page out of key order: nothing was paid/,
    )
    equal(reads, 1)
  })
})
