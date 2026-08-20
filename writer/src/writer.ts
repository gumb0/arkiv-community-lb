// Generic entity write operations over the official Arkiv SDK.
// Schema-blind: payloads are opaque bytes, attributes are generic name→value
// pairs — domain encoding belongs to the caller (see AGENTS.md).

import {
  type AttributeInputs,
  type CreationFlags,
  createWalletClient,
  type Expiry,
} from "@arkiv-network/sdk"
import {
  createPublicClient,
  defineChain,
  http,
  type Chain,
  type Hex,
} from "viem"
import { privateKeyToAccount } from "viem/accounts"

// Callers build these with the SDK's own vocabulary: tagged value
// constructors (i32, u256, str, …) and ExpirationTime helpers.
export type { AttributeInputs, CreationFlags, Expiry }

export type CreateOp = {
  payload: Uint8Array
  contentType: string
  attributes?: AttributeInputs
  expires: Expiry
  flags?: CreationFlags
  salt?: bigint
}

export type PatchOp = {
  entityKey: Hex
  set?: AttributeInputs
  unset?: readonly string[]
  payload?: Uint8Array
  contentType?: string
}

export type ExtendOp = { entityKey: Hex; expires: Expiry }
export type DeleteOp = { entityKey: Hex }

export type BatchOps = {
  creates?: CreateOp[]
  patches?: PatchOp[]
  deletes?: DeleteOp[]
  extensions?: ExtendOp[]
}

export type WriterConfig = {
  rpcUrl: string
  /** Sent as `Authorization: Bearer …` when the endpoint is metered. */
  apiKey?: string
  privateKey: Hex
  /**
   * How often to ask whether a transaction has been mined. viem's own default
   * is 4 s, which on a 2 s chain waits longer than the block it waits for.
   */
  pollingInterval?: number
}

/** Connects, probes the chain id, and returns the writer surface. */
export async function createWriter(config: WriterConfig) {
  const transport = http(config.rpcUrl, {
    fetchOptions: config.apiKey
      ? { headers: { authorization: `Bearer ${config.apiKey}` } }
      : undefined,
  })

  const chainId = await createPublicClient({ transport }).getChainId()

  const chain: Chain = defineChain({
    id: chainId,
    name: `arkiv-${chainId}`,
    nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
    rpcUrls: { default: { http: [config.rpcUrl] } },
  })

  const account = privateKeyToAccount(config.privateKey)
  const wallet = createWalletClient({
    chain,
    transport,
    account,
    pollingInterval: config.pollingInterval ?? 1000,
  })

  return {
    address: account.address,
    chainId,

    createEntity: (op: CreateOp) => wallet.createEntity(op),
    patchEntity: (op: PatchOp) => wallet.patchEntity(op),
    deleteEntity: (op: DeleteOp) => wallet.deleteEntity(op),
    extendEntity: (op: ExtendOp) => wallet.extendEntity(op),
    /** All operations in one transaction — one nonce, atomic on-chain. */
    executeBatch: (ops: BatchOps) => wallet.executeBatch(ops),
  }
}

export type Writer = Awaited<ReturnType<typeof createWriter>>
