// Generic entity write operations over the official Arkiv SDK.
// Schema-blind: an operation carries the schema it is typed by, built by
// the caller from the attributes it sends — domain encoding belongs to the
// caller (see AGENTS.md).

import {
  type CreateEntityParameters,
  createWalletClient,
  type DeleteEntityParameters,
  type ExecuteBatchParameters,
  type ExtendEntityParameters,
  type PatchEntityParameters,
} from "@arkiv-network/sdk"
import {
  createPublicClient,
  defineChain,
  http,
  type Chain,
  type Hex,
} from "viem"
import { privateKeyToAccount } from "viem/accounts"

// The SDK's own parameter shapes, so an operation goes into a batch as it
// is. Callers build them with the SDK's vocabulary: defineSchema and the
// type constructors (i32, u256, str, …) for columns and values,
// ExpirationTime for lifetimes.
export type CreateOp = CreateEntityParameters
export type PatchOp = PatchEntityParameters
export type ExtendOp = ExtendEntityParameters
export type DeleteOp = DeleteEntityParameters
export type BatchOps = ExecuteBatchParameters

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

    createEntity: (op: CreateOp) => wallet.createEntity(op.schema, op.values, op),
    patchEntity: (op: PatchOp) => wallet.patchEntity(op.schema, op.entityKey, op),
    deleteEntity: (op: DeleteOp) => wallet.deleteEntity(op),
    extendEntity: (op: ExtendOp) => wallet.extendEntity(op),
    /** All operations in one transaction — one nonce, atomic on-chain. */
    executeBatch: (ops: BatchOps) => wallet.executeBatch(ops),
  }
}

export type Writer = Awaited<ReturnType<typeof createWriter>>
