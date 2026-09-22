// A chain the ledger runs over in tests: counter records and receipts
// as the LB and settle write them, answered by what a query names.

import type { Entity } from "@arkiv-network/sdk"
import { stringToBytes, type Hex } from "viem"
import type { Reader } from "../src/chain.ts"
import { KIND } from "../src/records.ts"

export const LB = "0x1111111111111111111111111111111111111111" as Hex
export const SETTLE = "0x2222222222222222222222222222222222222222" as Hex

export function provider(n: number): Hex {
  return `0x${String(n).repeat(40)}`.slice(0, 42) as Hex
}

export function key(n: number): Hex {
  return `0x${n.toString(16).padStart(64, "0")}` as Hex
}

function entity(
  entityKey: Hex,
  creator: Hex,
  attributes: Record<string, { type: string; value: unknown }>,
  payload: unknown,
): Entity {
  return {
    key: entityKey,
    creator,
    attributes,
    payload: stringToBytes(JSON.stringify(payload)),
  } as unknown as Entity
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
}): Entity {
  return entity(
    options.key,
    options.creator ?? LB,
    {
      kind: { type: "str", value: KIND.counter },
      v: { type: "i32", value: 1 },
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

/** A receipt, as a run leaves it behind. */
export function receipt(options: {
  key: Hex
  counter: Hex
  provider: Hex
  amountWei?: bigint
  creator?: Hex
}): Entity {
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
 * Answers a query with the rows whose kind, creator and provider it
 * names. Every query a test asks is recorded, so a test can hold the
 * ledger to one read per provider.
 */
export function fakeChain(rows: Entity[]): Reader & { asked: string[] } {
  const asked: string[] = []
  return {
    chainId: 7738577,
    asked,
    query: async (text: string) => {
      asked.push(text)
      return rows.filter((row) => {
        const attributes = row.attributes as Record<string, { value: unknown }>
        const kind = attributes.kind?.value as string
        if (!text.includes(`str('${kind}')`)) return false
        const creator = (row.creator as string).toLowerCase()
        if (!text.includes(creator)) return false
        const provider = (attributes.provider?.value as string).toLowerCase()
        return !text.includes("provider = addr(") || text.includes(`provider = addr(${provider})`)
      })
    },
  }
}
