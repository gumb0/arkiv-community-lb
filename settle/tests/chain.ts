// A chain the ledger runs over in tests: counter records and receipts
// as the LB and settle write them, answered by what a query names.

import { stringToHex, type Hex } from "viem"
import type { Attribute, Reader, Row } from "../src/chain.ts"
import { KIND, type ReceiptRecord } from "../src/records.ts"

export const LB = "0x1111111111111111111111111111111111111111" as Hex
export const SETTLE = "0x2222222222222222222222222222222222222222" as Hex

export function address(n: number): Hex {
  return `0x${String(n).repeat(40)}`.slice(0, 42) as Hex
}

export function key(n: number): Hex {
  return `0x${n.toString(16).padStart(64, "0")}` as Hex
}

function entity(
  entityKey: Hex,
  creator: Hex,
  attributes: Record<string, Attribute>,
  payload: unknown,
): Row {
  return {
    key: entityKey,
    creator,
    attributes: { ...attributes, payload: document(payload) },
  }
}

/** A JSON document as the bytes attribute that carries it: hex on the wire. */
export function document(value: unknown): Attribute {
  return { type: "bytes", value: stringToHex(JSON.stringify(value)) }
}

/** A closed counter record, as the LB's flush leaves it. */
export function closedCounter(options: {
  key: Hex
  provider: Hex
  agreement?: Hex
  count: number
  weiPerCall?: bigint
  openedBlock?: number
  closedBlock?: number
  creator?: Hex
  /** A record written at a schema version this code does not read. */
  version?: number
}): Row {
  return entity(
    options.key,
    options.creator ?? LB,
    {
      kind: { type: "str", value: KIND.counter },
      v: { type: "i32", value: options.version ?? 1 },
      agreement: { type: "key", value: options.agreement ?? key(0xaa) },
      provider: { type: "addr", value: options.provider },
      state: { type: "str", value: "closed" },
    },
    {
      count: options.count,
      wei_per_call: (options.weiPerCall ?? 10n ** 15n).toString(),
      opened_block: options.openedBlock ?? 100,
      closed_block: options.closedBlock ?? 200,
    },
  )
}

/** A record still counting: open, with no closing block. */
export function openCounter(options: { key: Hex; provider: Hex; count: number }): Row {
  const record = closedCounter(options)
  record.attributes.state = { type: "str", value: "open" }
  record.attributes.payload = document({
    count: options.count,
    wei_per_call: (10n ** 15n).toString(),
    opened_block: 100,
  })
  return record
}

/**
 * A receipt a run wrote, as a row the chain would answer with. The
 * attributes are the SDK's own value objects, which is the shape a
 * row carries, so what settle writes can be read back by what settle
 * reads.
 */
export function written(record: ReceiptRecord, entityKey: Hex, creator: Hex = SETTLE): Row {
  return {
    key: entityKey,
    creator,
    attributes: record.values as Record<string, Attribute>,
  }
}

/** A receipt, as a run leaves it behind. */
export function receipt(options: {
  key: Hex
  counter: Hex
  provider: Hex
  amountWei?: bigint
  creator?: Hex
}): Row {
  return entity(
    options.key,
    options.creator ?? SETTLE,
    {
      kind: { type: "str", value: KIND.receipt },
      v: { type: "i32", value: 1 },
      counter: { type: "key", value: options.counter },
      provider: { type: "addr", value: options.provider },
    },
    {
      agreement: key(0xaa),
      count: 1,
      wei_per_call: "1000000000000000",
      amount_wei: (options.amountWei ?? 10n ** 15n).toString(),
      payout: { chain_id: 560048, tx: key(0xff) },
    },
  )
}

/**
 * Answers a query with the rows that match every condition it names,
 * so a condition left out of a query shows up as rows that should not
 * have been answered with. Conditions are `name = type(value)`, the
 * shape the record module builds, and `$creator` is the row's writer.
 * Every query asked is recorded, so a test can hold the ledger to one
 * read per provider.
 */
export function fakeChain(rows: Row[]): Reader & { asked: string[] } {
  const asked: string[] = []
  return {
    chainId: 7738577,
    asked,
    balance: async () => 10n ** 18n,
    query: async (text: string) => {
      asked.push(text)
      const conditions = text.split(" AND ")
      return rows.filter((row) => conditions.every((condition) => matches(row, condition)))
    },
  }
}

/**
 * One condition of a query against one row. A condition is
 * `name = type(value)`, the shape the record module builds, and
 * `$creator` is the row's writer rather than an attribute of it.
 */
function matches(row: Row, condition: string): boolean {
  const [name = "", wrapped = ""] = condition.trim().split(" = ")
  const value = wrapped.slice(wrapped.indexOf("(") + 1, -1).replaceAll("'", "")
  const held = name === "$creator" ? row.creator : row.attributes[name]?.value
  return String(held).toLowerCase() === value.toLowerCase()
}
