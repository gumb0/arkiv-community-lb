// Arkiv as settle sees it: queries only, every page of them. Behind a
// small surface, so the ledger is tested over a fake.

import { createPublicClient, createWalletClient } from "@arkiv-network/sdk"
import { defineChain, http, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"
import type { ReceiptRecord } from "./records.ts"

/** One attribute of a row: the node's type tag and the decoded value. */
export type Attribute = { type: string; value: unknown }

/**
 * A row as settle reads it: the entity's key and creator, and its
 * attributes by name, the payload among them. The one shape the rest
 * of settle sees, whatever the SDK's rows look like.
 */
export type Row = {
  key: Hex
  creator: Hex
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
const PAGE = 100

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
      everyPage(async (offset) => {
        const page = await client.query(text, {
          limit: PAGE,
          offset,
          count: true,
          select: { $key: true, $creator: true, $attributes: true },
        })
        if (page.count === undefined) {
          throw new Error("the node did not say how many rows the query matches")
        }
        return {
          entities: page.entities.map((flat) => ({
            key: flat.$key,
            creator: flat.$creator,
            attributes: flat.$attributes as Record<string, Attribute>,
          })),
          count: Number(page.count),
        }
      }),
  }
}

/** One page of a query, and how many rows the whole query matches. */
export type Page = { entities: Row[]; count: number }

/** How many times a query is walked before the run gives up. */
const WALK_TRIES = 3

/**
 * Every row of a query. Settle reads what has piled up since its last
 * run, which is not bounded by the provider cap the LB's own reads rely
 * on.
 *
 * Pages go by offset, so a row that expires during a walk shifts the
 * rows after it and one is skipped, and a skipped receipt would read as
 * an unpaid record. If the count changes, the walk starts over. After
 * three walks the run stops, before anything is paid. If a row expires
 * and another appears, the count stays the same and the skipped row is
 * not noticed. A skipped closed record is paid by the next run. Receipts
 * are only written by settle, so this needs two runs at the same time
 * (docs/LIMITATIONS.md).
 */
export async function everyPage(read: (offset: number) => Promise<Page>): Promise<Row[]> {
  for (let attempt = 0; attempt < WALK_TRIES; attempt += 1) {
    const rows = await walk(read)
    if (rows !== undefined) return rows
  }
  throw new Error(`a query's rows kept changing over ${WALK_TRIES} reads: nothing was paid`)
}

/** One walk from the first page, or undefined if the rows changed during it. */
async function walk(read: (offset: number) => Promise<Page>): Promise<Row[] | undefined> {
  const first = await read(0)
  const rows = [...first.entities]
  while (rows.length < first.count) {
    const page = await read(rows.length)
    // An empty page while the count says more rows exist: the two answers
    // disagree, so the rows changed. The offset would not move either, and
    // the same read would repeat forever.
    if (page.count !== first.count || page.entities.length === 0) return undefined
    rows.push(...page.entities)
  }
  return rows
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
