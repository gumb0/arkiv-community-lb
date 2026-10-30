// Arkiv as settle sees it: queries only, every page of them. Behind a
// small surface, so the ledger is tested over a fake.

import { createPublicClient, createWalletClient, type Entity } from "@arkiv-network/sdk"
import { defineChain, http, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"
import type { ReceiptRecord } from "./records.ts"

/** One attribute of a row: the node's type tag and the decoded value. */
export type Attribute = { type: string; value: unknown }

/**
 * A row as settle reads it: the entity's key and creator, its payload,
 * and its attributes by name. The one shape the rest of settle sees,
 * whatever the SDK's rows look like.
 */
export type Row = {
  key: Hex
  creator: Hex
  payload: Uint8Array
  attributes: Record<string, Attribute>
}

export type Reader = {
  chainId: number
  /** Every row the query matches, following the node's pages. */
  query(text: string): Promise<Row[]>
  /** What an address holds on Arkiv, which is what a write costs. */
  balance(address: Hex): Promise<bigint>
}

/** The node's page maximum. Asking for more is an error. */
const PAGE = 200

export async function connectReader(rpcUrl: string, apiKey?: string): Promise<Reader> {
  const transport = http(rpcUrl, {
    fetchOptions: apiKey ? { headers: { authorization: `Bearer ${apiKey}` } } : undefined,
  })
  const client = createPublicClient({ transport })
  const chainId = await client.getChainId()
  return {
    chainId,
    balance: (address) => client.getBalance({ address }),
    query: (text) =>
      everyPage(async (cursor) => {
        const page = await client.query(text, {
          limit: PAGE,
          cursor,
          select: { key: true, creator: true, payload: true, attributes: true },
        })
        return { entities: page.entities.map(row), cursor: page.cursor }
      }),
  }
}

/** The SDK's entity as a row, with every selected part present. */
function row(entity: Entity): Row {
  if (entity.payload === undefined) {
    throw new Error(`record ${entity.key} came without its payload`)
  }
  return {
    key: entity.key as Hex,
    creator: entity.creator as Hex,
    payload: entity.payload,
    attributes: (entity.attributes ?? {}) as Record<string, Attribute>,
  }
}

/** One page of a query: what it holds, and where the next one starts. */
export type Page = { entities: Row[]; cursor: string | undefined }

/**
 * Every page of a query, from the first to the one that names no
 * next. Settle reads what has piled up since its last run, which is
 * not bounded by the provider cap the LB's own reads rely on.
 */
export async function everyPage(
  read: (cursor: string | undefined) => Promise<Page>,
): Promise<Row[]> {
  const entities: Row[] = []
  let cursor: string | undefined
  do {
    const page = await read(cursor)
    entities.push(...page.entities)
    cursor = page.cursor
  } while (cursor !== undefined)
  return entities
}

/**
 * The receipts a run writes, signed with settle's own key. Written
 * with the SDK directly rather than through the LB's writer sidecar:
 * settle is an audit tool, and it never depends on the LB's stack
 * being up.
 */
export type ReceiptWriter = {
  address: Hex
  /** One transaction, so a batch lands whole or not at all. */
  write(records: ReceiptRecord[]): Promise<void>
}

export async function connectWriter(
  rpcUrl: string,
  apiKey: string | undefined,
  privateKey: Hex,
): Promise<ReceiptWriter> {
  const transport = http(rpcUrl, {
    fetchOptions: apiKey ? { headers: { authorization: `Bearer ${apiKey}` } } : undefined,
  })
  const chainId = await createPublicClient({ transport }).getChainId()
  const account = privateKeyToAccount(privateKey)
  const wallet = createWalletClient({
    chain: defineChain({
      id: chainId,
      name: `arkiv-${chainId}`,
      nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
      rpcUrls: { default: { http: [rpcUrl] } },
    }),
    transport,
    account,
    pollingInterval: 1000,
  })
  return {
    address: account.address,
    // A receipt is written exactly as `receiptFor` built it, lifetime
    // and flags included: what a receipt is belongs in one place.
    write: async (records) => {
      await wallet.executeBatch({ creates: records })
    },
  }
}
