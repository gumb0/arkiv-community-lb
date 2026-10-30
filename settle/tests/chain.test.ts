// Reading a query's pages to the end. Run: npm test

import { deepStrictEqual as deepEqual, rejects, strictEqual as equal } from "node:assert/strict"
import { describe, it } from "node:test"
import { everyPage, type Page } from "../src/chain.ts"
import { address, closedCounter, key } from "./chain.ts"

const row = (n: number) => closedCounter({ key: key(n), provider: address(1), count: n })

describe("reading every page", () => {
  it("walks by offset until it has every row the count names", async () => {
    const pages: Page[] = [
      { entities: [row(1), row(2)], count: 4 },
      { entities: [row(3)], count: 4 },
      { entities: [row(4)], count: 4 },
    ]
    const asked: number[] = []

    const entities = await everyPage(async (offset) => {
      asked.push(offset)
      return pages[asked.length - 1] as Page
    })

    deepEqual(
      entities.map((entity) => entity.key),
      [key(1), key(2), key(3), key(4)],
      "in the order the pages came",
    )
    deepEqual(asked, [0, 2, 3], "each page starts after the rows already read")
  })

  it("reads one page when that is all there is", async () => {
    let reads = 0
    const entities = await everyPage(async () => {
      reads += 1
      return { entities: [row(1)], count: 1 }
    })

    equal(reads, 1)
    equal(entities.length, 1)
  })

  it("asks once for an empty answer", async () => {
    let reads = 0
    const entities = await everyPage(async () => {
      reads += 1
      return { entities: [], count: 0 }
    })

    equal(reads, 1)
    deepEqual(entities, [])
  })

  it("starts over when the count changes during a walk", async () => {
    // A row expired between the two pages: what the first page read is
    // no longer a prefix of the rest, so the walk is thrown away.
    const pages: Page[] = [
      { entities: [row(1), row(2)], count: 4 },
      { entities: [row(4)], count: 3 },
      { entities: [row(1), row(3)], count: 3 },
      { entities: [row(4)], count: 3 },
    ]
    const asked: number[] = []

    const entities = await everyPage(async (offset) => {
      asked.push(offset)
      return pages[asked.length - 1] as Page
    })

    deepEqual(
      entities.map((entity) => entity.key),
      [key(1), key(3), key(4)],
    )
    deepEqual(asked, [0, 2, 0, 2])
  })

  it("starts over when the rows run out before the count", async () => {
    const pages: Page[] = [
      { entities: [row(1)], count: 2 },
      { entities: [], count: 2 },
      { entities: [row(1)], count: 1 },
    ]
    let reads = 0

    const entities = await everyPage(async () => {
      reads += 1
      return pages[reads - 1] as Page
    })

    equal(entities.length, 1)
    equal(reads, 3, "the empty page restarted the walk")
  })

  it("refuses the run when the count changes on every walk", async () => {
    let reads = 0
    await rejects(
      everyPage(async () => {
        reads += 1
        return { entities: [row(reads)], count: 100 + reads }
      }),
      /kept changing over 3 reads: nothing was paid/,
    )
    equal(reads, 6, "three walks of two reads")
  })
})
