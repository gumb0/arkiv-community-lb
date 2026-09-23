// Arkiv as settle sees it: queries only, every page of them. Behind a
// small surface, so the ledger is tested over a fake.

import { createPublicClient, type Entity } from "@arkiv-network/sdk"
import { http } from "viem"

export type Reader = {
  chainId: number
  /** Every entity the query matches, following the node's pages. */
  query(text: string): Promise<Entity[]>
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
    query: (text) =>
      everyPage((cursor) =>
        client.query(text, {
          limit: PAGE,
          cursor,
          select: { key: true, creator: true, payload: true, attributes: true },
        }),
      ),
  }
}

/** One page of a query: what it holds, and where the next one starts. */
export type Page = { entities: Entity[]; cursor: string | undefined }

/**
 * Every page of a query, from the first to the one that names no
 * next. Settle reads what has piled up since its last run, which is
 * not bounded by the provider cap the LB's own reads rely on.
 */
export async function everyPage(
  read: (cursor: string | undefined) => Promise<Page>,
): Promise<Entity[]> {
  const entities: Entity[] = []
  let cursor: string | undefined
  do {
    const page = await read(cursor)
    entities.push(...page.entities)
    cursor = page.cursor
  } while (cursor !== undefined)
  return entities
}
