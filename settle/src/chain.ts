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
  /**
   * Every row the query matches, following the node's pages. Pass it
   * without an `ORDER BY`: the walk sorts by key itself.
   */
  query(text: string): Promise<Row[]>
  /** What an address holds on Arkiv, which is what a write costs. */
  balance(address: Hex): Promise<bigint>
}

/** The node's page maximum. Asking for more is an error. */
export const PAGE = 100

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
      everyPage(async (after) => {
        // In parentheses, so the bound applies to the whole query and not
        // only to its last term after an OR.
        const page = await client.query(
          `(${text})${after !== undefined ? ` AND $key > key(${after})` : ""} ORDER BY $key`,
          { limit: PAGE, select: { $key: true, $creator: true, $attributes: true } },
        )
        return page.entities.map((flat) => ({
          key: flat.$key,
          creator: flat.$creator,
          attributes: flat.$attributes as Record<string, Attribute>,
        }))
      }),
  }
}

/**
 * Every row of a query, a page of keys at a time, each page the rows
 * after the last key read, until a page comes back empty.
 *
 * Settle reads what has piled up since its last run, which is not
 * bounded by the provider cap the LB's own reads rely on. A row that
 * expires during the walk takes no other row with it, as it would with
 * an offset. A row created during the walk with a key below the last
 * one read is not seen: a closed record is then paid by the next run,
 * and a receipt can only come from a second run at the same time, which
 * is not safe anyway (docs/LIMITATIONS.md).
 */
export async function everyPage(read: (after: Hex | undefined) => Promise<Row[]>): Promise<Row[]> {
  const rows: Row[] = []
  let after: Hex | undefined
  for (;;) {
    const page = await read(after)
    // Only an empty page is the end. A short one usually is too, but a
    // node that cut a page short, by size say, would then hide the rest,
    // and a receipt not read is a record paid twice.
    if (page.length === 0) return rows
    // Each key above the one before it, the first above the bound. A node
    // that ignored the bound would hand back the same page for ever, and
    // one that ignored the order would make the next bound skip rows.
    let previous = after
    for (const row of page) {
      if (previous !== undefined && BigInt(row.key) <= BigInt(previous)) {
        const from = after === undefined ? "the first page" : `the page after key ${after}`
        throw new Error(`the node answered ${from} out of key order: nothing was paid`)
      }
      previous = row.key
    }
    rows.push(...page)
    after = previous
  }
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
