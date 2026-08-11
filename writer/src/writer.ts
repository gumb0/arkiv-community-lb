// Generic entity write operations over the official Arkiv SDK.
// Schema-blind: payloads are opaque bytes, attributes are generic key-value
// pairs — domain encoding belongs to the caller (see AGENTS.md).

import { createWalletClient } from "@arkiv-network/sdk"
import {
  createPublicClient,
  defineChain,
  http,
  type Chain,
  type Hex,
} from "viem"
import { privateKeyToAccount } from "viem/accounts"

export type Attribute = { key: string; value: string | number }

export type CreateOp = {
  payload: Uint8Array
  contentType: string
  attributes: Attribute[]
  /** Seconds until expiry; must be a positive multiple of the block time. */
  expiresIn: number
}

export type UpdateOp = CreateOp & { entityKey: Hex }
export type ExtendOp = { entityKey: Hex; expiresIn: number }
export type DeleteOp = { entityKey: Hex }

export type BatchOps = {
  creates?: CreateOp[]
  updates?: UpdateOp[]
  deletes?: DeleteOp[]
  extensions?: ExtendOp[]
}

export type WriterConfig = {
  /** Full RPC endpoint URL, any access key already embedded. */
  rpcUrl: string
  privateKey: Hex
}

/** Connects, probes the chain id, and returns the writer surface. */
export async function createWriter(config: WriterConfig) {
  const transport = http(config.rpcUrl)

  const chainId = await createPublicClient({ transport }).getChainId()

  const chain: Chain = defineChain({
    id: chainId,
    name: `arkiv-${chainId}`,
    nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
    rpcUrls: { default: { http: [config.rpcUrl] } },
  })

  const account = privateKeyToAccount(config.privateKey)
  const wallet = createWalletClient({ chain, transport, account })

  return {
    address: account.address,
    chainId,

    createEntity: (op: CreateOp) => wallet.createEntity(op),
    updateEntity: (op: UpdateOp) => wallet.updateEntity(op),
    deleteEntity: (op: DeleteOp) => wallet.deleteEntity(op),
    extendEntity: (op: ExtendOp) => wallet.extendEntity(op),
    /** All operations in one transaction — one nonce, atomic on-chain. */
    mutateEntities: (ops: BatchOps) => wallet.mutateEntities(ops),
  }
}

export type Writer = Awaited<ReturnType<typeof createWriter>>
