// Reading a query's pages to the end. Run: npm test

import { deepStrictEqual as deepEqual, strictEqual as equal } from "node:assert/strict"
import { describe, it } from "node:test"
import { everyPage, type Page } from "../src/chain.ts"
import { closedCounter, key, provider } from "./chain.ts"

const row = (n: number) => closedCounter({ key: key(n), provider: provider(1), count: n })

describe("reading every page", () => {
  it("follows the cursor until a page names no next one", async () => {
    const pages: Page[] = [
      { entities: [row(1), row(2)], cursor: "a" },
      { entities: [row(3)], cursor: "b" },
      { entities: [row(4)], cursor: undefined },
    ]
    const asked: (string | undefined)[] = []

    const entities = await everyPage(async (cursor) => {
      asked.push(cursor)
      return pages[asked.length - 1] as Page
    })

    deepEqual(
      entities.map((entity) => entity.key),
      [key(1), key(2), key(3), key(4)],
      "in the order the pages came",
    )
    deepEqual(asked, [undefined, "a", "b"], "the first page asks for no cursor")
  })

  it("reads one page when that is all there is", async () => {
    let reads = 0
    const entities = await everyPage(async () => {
      reads += 1
      return { entities: [row(1)], cursor: undefined }
    })

    equal(reads, 1)
    equal(entities.length, 1)
  })

  it("asks once for an empty answer", async () => {
    let reads = 0
    const entities = await everyPage(async () => {
      reads += 1
      return { entities: [], cursor: undefined }
    })

    equal(reads, 1)
    deepEqual(entities, [])
  })
})
