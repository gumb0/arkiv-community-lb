// Batch-scale probe: N creates in one transaction, N extends in one
// transaction, N deletes in one transaction — wall time and gas per phase.
// Validates that batched cost is per transaction, not per operation, and
// measures gas per op at scale.
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY; BATCH_N (default 100).
// Run: npm run batch

import { jsonToPayload } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "./writer.ts"

const TTL_SECONDS = 120

function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name} (see .env.example)`)
    process.exit(1)
  }
  return value
}

const batchN = Number(process.env.BATCH_N ?? "100")

const rpcUrl = (() => {
  const base = env("RPC_URL").replace(/\/+$/, "")
  const key = env("API_KEY", false)
  return key ? `${base}/${key}` : base
})()

let rpcId = 0
async function rawRpc(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++rpcId, method, params }),
  })
  const body = (await res.json()) as { result?: unknown; error?: { message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message}`)
  return body.result
}

async function txStats(txHash: Hex): Promise<{ gasUsed: bigint; calldataBytes: number }> {
  const receipt = (await rawRpc("eth_getTransactionReceipt", [txHash])) as { gasUsed: Hex }
  const tx = (await rawRpc("eth_getTransactionByHash", [txHash])) as { input: Hex }
  return { gasUsed: BigInt(receipt.gasUsed), calldataBytes: (tx.input.length - 2) / 2 }
}

function report(phase: string, seconds: number, stats: { gasUsed: bigint; calldataBytes: number }) {
  console.log(
    `${phase}: ${seconds.toFixed(1)}s, gas ${stats.gasUsed} ` +
      `(${stats.gasUsed / BigInt(batchN)}/op), calldata ${stats.calldataBytes} bytes`,
  )
}

const writer = await createWriter({ rpcUrl, privateKey: env("PRIVATE_KEY") as Hex })
console.log(`batch experiment: N=${batchN}, address ${writer.address}, chain ${writer.chainId}\n`)

// Phase 1: N creates, one transaction.
let started = Date.now()
const { txHash: createTx, createdEntities } = await writer.mutateEntities({
  creates: Array.from({ length: batchN }, (_, i) => ({
    payload: jsonToPayload({ probe: "batch-experiment", i }),
    contentType: "application/json",
    attributes: [{ key: "kind", value: "batch-experiment" }],
    expiresIn: TTL_SECONDS,
  })),
})
report(`create x${batchN}`, (Date.now() - started) / 1000, await txStats(createTx))
if (createdEntities.length !== batchN) {
  throw new Error(`expected ${batchN} keys, got ${createdEntities.length}`)
}

// Phase 2: N extends, one transaction — the refresh-cycle shape at cap scale.
started = Date.now()
const { txHash: extendTx } = await writer.mutateEntities({
  extensions: createdEntities.map((entityKey) => ({ entityKey, expiresIn: TTL_SECONDS })),
})
report(`extend x${batchN}`, (Date.now() - started) / 1000, await txStats(extendTx))

// Phase 3: N deletes, one transaction — cleanup doubling as a measurement.
started = Date.now()
const { txHash: deleteTx } = await writer.mutateEntities({
  deletes: createdEntities.map((entityKey) => ({ entityKey })),
})
report(`delete x${batchN}`, (Date.now() - started) / 1000, await txStats(deleteTx))

console.log("\nbatch experiment done")
