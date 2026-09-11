// Gas of per-agreement counters records at the provider cap: N creates
// shaped like counters records, one batch of N count patches, one batch
// of N closing patches (count and state), and N receipts. Reports gasUsed
// per operation and calldata per batch, which is what a flush at N
// agreements would cost. A create is about 1.7 KB of calldata, so the
// creates go in batches that stay under the node's 128 KiB transaction
// cap; the patches fit in one.
//
// Env: WRITER_PRIVATE_KEY_FILE, ARKIV_RPC_URL, optional ARKIV_API_KEY;
// COUNTERS_N (default 100).
// Run: npm run counters

import { addr, ExpirationTime, i32, jsonToPayload, key, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"
import { env, privateKeyFromFile } from "../src/env.ts"

const n = Number(process.env.COUNTERS_N ?? "100")
const rpcUrl = env("ARKIV_RPC_URL").replace(/\/+$/, "")
const apiKey = env("ARKIV_API_KEY", false)
const RUN = `r${Date.now().toString(36)}`

let rpcId = 0
async function rawRpc(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++rpcId, method, params }),
  })
  const body = (await res.json()) as { result?: unknown; error?: { message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message}`)
  return body.result
}

async function report(phase: string, txHash: Hex, ops: number) {
  const receipt = (await rawRpc("eth_getTransactionReceipt", [txHash])) as { gasUsed: Hex }
  const tx = (await rawRpc("eth_getTransactionByHash", [txHash])) as { input: Hex }
  const gas = BigInt(receipt.gasUsed)
  console.log(
    `${phase} x${ops}: gas ${gas} (${gas / BigInt(ops)}/op), calldata ${(tx.input.length - 2) / 2} bytes`,
  )
}

/** Creates in batches under the transaction size cap, returning every key. */
async function createInBatches(
  phase: string,
  creates: Parameters<typeof writer.executeBatch>[0]["creates"] & unknown[],
): Promise<Hex[]> {
  const keys: Hex[] = []
  for (let from = 0; from < creates.length; from += CREATES_PER_BATCH) {
    const slice = creates.slice(from, from + CREATES_PER_BATCH)
    const { txHash, createdEntities } = await writer.executeBatch({ creates: slice })
    await report(phase, txHash, slice.length)
    keys.push(...createdEntities)
  }
  return keys
}

const CREATES_PER_BATCH = 70

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: privateKeyFromFile(),
})
console.log(`counters probe: N=${n}, run ${RUN}, address ${writer.address}, chain ${writer.chainId}\n`)

// Agreement keys and provider addresses that look real: 32 and 20 bytes.
const agreementKey = (i: number) => `0x${(i + 1).toString(16).padStart(64, "0")}` as Hex
const provider = (i: number) => `0x${(i + 1).toString(16).padStart(40, "0")}` as Hex
const wei = "1000000000000000"

const createdEntities = await createInBatches(
  "counters create",
  Array.from({ length: n }, (_, i) => ({
    payload: jsonToPayload({ count: 48213 + i, wei_per_call: wei }),
    contentType: "application/json",
    attributes: {
      kind: str("rpc.counters"),
      v: i32(1),
      agreement: key(agreementKey(i)),
      provider: addr(provider(i)),
      state: str("open"),
    },
    expires: ExpirationTime.fromBlocks(300),
  })),
)

const { txHash: patchTx } = await writer.executeBatch({
  patches: createdEntities.map((entityKey, i) => ({
    entityKey,
    payload: jsonToPayload({ count: 96426 + i, wei_per_call: wei }),
  })),
})
await report("counters patch (count)", patchTx, n)

const { txHash: closeTx } = await writer.executeBatch({
  patches: createdEntities.map((entityKey, i) => ({
    entityKey,
    payload: jsonToPayload({ count: 144639 + i, wei_per_call: wei }),
    set: { state: str("closed") },
  })),
})
await report("counters close (count and state)", closeTx, n)

const receipts = await createInBatches(
  "receipt create",
  createdEntities.map((countersKey, i) => ({
    payload: jsonToPayload({
      agreement: agreementKey(i),
      count: 144639 + i,
      wei_per_call: wei,
      amount_wei: "144639000000000000000",
      payout: { chain_id: 560048, tx: `0x${"ab".repeat(32)}` },
    }),
    contentType: "application/json",
    attributes: {
      kind: str("rpc.receipt"),
      v: i32(1),
      counters: key(countersKey),
      provider: addr(provider(i)),
    },
    expires: ExpirationTime.fromBlocks(300),
  })),
)

await writer.executeBatch({
  deletes: [...createdEntities, ...receipts].map((entityKey) => ({ entityKey })),
})
console.log("\ncounters probe done")
