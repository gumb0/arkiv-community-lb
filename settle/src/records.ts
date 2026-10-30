// The records settle reads and writes: the LB's counter records, and
// its own receipts. The one place a record's shape is spelled here.

import {
  addr,
  bytes,
  type CreateEntityParameters,
  defineSchema,
  ExpirationTime,
  i32,
  key as keyAttribute,
  str,
} from "@arkiv-network/sdk"
import { hexToString, stringToHex, type Hex } from "viem"
import type { Row } from "./chain.ts"

const SCHEMA_VERSION = 1

export const KIND = {
  counter: "rpc.counter",
  receipt: "rpc.receipt",
} as const

// --- query text ------------------------------------------------------------
// Every query starts from a kind at this schema version, so a record
// written by newer code never takes a row.

export function byKind(kind: string, ...conditions: string[]): string {
  return [`kind = str('${kind}')`, `v = i32(${SCHEMA_VERSION})`, ...conditions].join(" AND ")
}

export const creator = (address: Hex): string => `$creator = addr(${address.toLowerCase()})`
export const attrAddr = (name: string, address: Hex): string =>
  `${name} = addr(${address.toLowerCase()})`
export const attrStr = (name: string, value: string): string => `${name} = str('${value}')`

// --- reading ---------------------------------------------------------------

function attribute(row: Row, name: string, type: string): unknown {
  const value = row.attributes[name]
  if (value === undefined || value.type !== type) {
    throw new Error(`record ${row.key} has no ${type} attribute ${name}`)
  }
  return value.value
}

// A record's JSON document rides in the bytes attribute named payload
// (docs/ENTITIES.md), hex on the wire.
function payloadJson(row: Row): Record<string, unknown> {
  const payload = row.attributes.payload
  if (payload === undefined || payload.type !== "bytes") {
    throw new Error(`record ${row.key} came without its payload`)
  }
  return JSON.parse(hexToString(payload.value as Hex)) as Record<string, unknown>
}

/** One settlement period of one agreement, closed and payable. */
export type Counter = {
  key: Hex
  agreement: Hex
  provider: Hex
  count: bigint
  weiPerCall: bigint
  openedBlock: bigint
  closedBlock: bigint
}

export function decodeCounter(row: Row): Counter {
  const payload = payloadJson(row)
  const closed = payload.closed_block
  if (closed === undefined || closed === null) {
    throw new Error(`counter record ${row.key} is closed but names no closing block`)
  }
  return {
    key: row.key,
    agreement: attribute(row, "agreement", "key") as Hex,
    provider: attribute(row, "provider", "addr") as Hex,
    count: BigInt(payload.count as number),
    weiPerCall: BigInt(payload.wei_per_call as string),
    openedBlock: BigInt(payload.opened_block as number),
    closedBlock: BigInt(closed as number),
  }
}

/** Settle's own record that one counter record was paid. */
export type Receipt = {
  key: Hex
  counter: Hex
  provider: Hex
  amountWei: bigint
  payout: { chainId: number; tx: Hex }
}

export function decodeReceipt(row: Row): Receipt {
  const payload = payloadJson(row)
  const payout = payload.payout as { chain_id: number; tx: Hex }
  return {
    key: row.key,
    counter: attribute(row, "counter", "key") as Hex,
    provider: attribute(row, "provider", "addr") as Hex,
    amountWei: BigInt(payload.amount_wei as string),
    payout: { chainId: payout.chain_id, tx: payout.tx },
  }
}

// --- writing ---------------------------------------------------------------

/**
 * The receipt's attributes as the SDK writes them: every pointer and
 * the kind indexed, the payload a blob. Unnamed, so the entity carries
 * these attributes and no schema tag, like every other record.
 */
export const RECEIPT = defineSchema({
  kind: str().indexed(),
  v: i32().indexed(),
  counter: keyAttribute().indexed(),
  provider: addr().indexed(),
  payload: bytes(),
})

/** A receipt as settle writes it: the whole create, nothing added later. */
export type ReceiptRecord = CreateEntityParameters<typeof RECEIPT>

/**
 * The receipt for one counter record, paid in full by one transfer.
 * The count and the rate are copied rather than pointed at: the
 * receipt is permanent and the counter record is not, so it has to
 * carry what it was paid for.
 */
export function receiptFor(record: Counter, payout: { chainId: number; tx: Hex }): ReceiptRecord {
  return {
    schema: RECEIPT,
    values: {
      kind: str(KIND.receipt),
      v: i32(SCHEMA_VERSION),
      counter: keyAttribute(record.key),
      provider: addr(record.provider),
      payload: bytes(
        stringToHex(
          JSON.stringify({
            agreement: record.agreement,
            count: Number(record.count),
            wei_per_call: record.weiPerCall.toString(),
            amount_wei: (record.count * record.weiPerCall).toString(),
            payout: { chain_id: payout.chainId, tx: payout.tx },
          }),
        ),
      ),
    },
    // A receipt has to outlive the record it paid for: settle reads a
    // record as unpaid until one of its own receipts names it, so a
    // receipt that expired first would have the next run pay again.
    expiration: ExpirationTime.permanent(),
    // And not even its writer may change one afterwards.
    flags: { readonly: true },
  }
}
