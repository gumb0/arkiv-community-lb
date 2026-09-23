// The records settle reads and writes: the LB's counter records, and
// its own receipts. The one place a record's shape is spelled here.

import {
  addr,
  i32,
  jsonToPayload,
  key as keyAttribute,
  str,
  type AttributeInputs,
  type Entity,
} from "@arkiv-network/sdk"
import { bytesToString, type Hex } from "viem"

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

function attribute(entity: Entity, name: string, type: string): unknown {
  const value = entity.attributes?.[name]
  if (value === undefined || value.type !== type) {
    throw new Error(`record ${entity.key} has no ${type} attribute ${name}`)
  }
  return value.value
}

function payloadJson(entity: Entity): Record<string, unknown> {
  if (entity.payload === undefined) {
    throw new Error(`record ${entity.key} came without its payload`)
  }
  return JSON.parse(bytesToString(entity.payload)) as Record<string, unknown>
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

export function decodeCounter(entity: Entity): Counter {
  const payload = payloadJson(entity)
  const closed = payload.closed_block
  if (closed === undefined || closed === null) {
    throw new Error(`counter record ${entity.key} is closed but names no closing block`)
  }
  return {
    key: entity.key as Hex,
    agreement: attribute(entity, "agreement", "key") as Hex,
    provider: attribute(entity, "provider", "addr") as Hex,
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

export function decodeReceipt(entity: Entity): Receipt {
  const payload = payloadJson(entity)
  const payout = payload.payout as { chain_id: number; tx: Hex }
  return {
    key: entity.key as Hex,
    counter: attribute(entity, "counter", "key") as Hex,
    provider: attribute(entity, "provider", "addr") as Hex,
    amountWei: BigInt(payload.amount_wei as string),
    payout: { chainId: payout.chain_id, tx: payout.tx },
  }
}

// --- writing ---------------------------------------------------------------

/** A receipt as settle writes it: attributes, payload, permanent. */
export type ReceiptRecord = {
  attributes: AttributeInputs
  payload: Uint8Array
  contentType: "application/json"
}

/**
 * The receipt for one counter record, paid in full by one transfer.
 * The count and the rate are copied rather than pointed at: the
 * receipt is permanent and the counter record is not, so it has to
 * carry what it was paid for.
 */
export function receiptFor(record: Counter, payout: { chainId: number; tx: Hex }): ReceiptRecord {
  return {
    attributes: {
      kind: str(KIND.receipt),
      v: i32(SCHEMA_VERSION),
      counter: keyAttribute(record.key),
      provider: addr(record.provider),
    },
    payload: jsonToPayload({
      agreement: record.agreement,
      count: Number(record.count),
      wei_per_call: record.weiPerCall.toString(),
      amount_wei: (record.count * record.weiPerCall).toString(),
      payout: { chain_id: payout.chainId, tx: payout.tx },
    }),
    contentType: "application/json",
  }
}
