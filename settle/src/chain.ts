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
    query: async (text) => {
      const entities: Entity[] = []
      let cursor: string | undefined
      // Settle reads what has piled up since its last run, which is
      // not bounded by the provider cap the LB's own reads rely on.
      do {
        const page = await client.query(text, {
          limit: PAGE,
          cursor,
          select: { key: true, creator: true, payload: true, attributes: true },
        })
        entities.push(...page.entities)
        cursor = page.cursor
      } while (cursor !== undefined)
      return entities
    },
  }
}
