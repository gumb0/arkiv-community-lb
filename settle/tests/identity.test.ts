// Whose receipts count as paid. Run: npm test

import { deepStrictEqual as deepEqual, throws } from "node:assert/strict"
import { describe, it } from "node:test"
import { previousAddresses } from "../src/identity.ts"

const CURRENT = "0x2222222222222222222222222222222222222222"
const OLD = "0x8888888888888888888888888888888888888888"
const OLDER = "0x9999999999999999999999999999999999999999"

describe("the previous settle addresses", () => {
  it("are none when nothing is configured", () => {
    deepEqual(previousAddresses(CURRENT, undefined), [])
    deepEqual(previousAddresses(CURRENT, ""), [])
  })

  it("are read from a comma-separated list, spaces allowed", () => {
    deepEqual(previousAddresses(CURRENT, `${OLD}, ${OLDER}`), [OLD, OLDER])
  })

  it("leave out the current address and repeats", () => {
    deepEqual(previousAddresses(CURRENT, `${OLD},${CURRENT},${OLD}`), [OLD])
  })

  it("refuse what is not an address", () => {
    throws(() => previousAddresses(CURRENT, `${OLD},0x12`), /0x12/)
  })
})
