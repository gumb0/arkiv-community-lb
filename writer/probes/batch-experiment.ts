// Batch-scale tariff probe on the 0.8 API: N-op transactions, one phase
// per op kind — wall time, gas (total and per op), calldata size. The
// patch phases run twice (one vs three mutations) to separate per-op
// from per-mutation pricing.
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY (sent as Authorization: Bearer);
// BATCH_N (default 100).
// Run: npm run batch
//
// The node caps raw transaction size at 128 KiB (txpool default), so a
// large-N create phase can exceed it regardless of endpoint — the error is
// "oversized data". Lower BATCH_N in that case; a gateway in front may
// reject the same batch earlier as HTTP 413.

import { ExpirationTime, i32, jsonToPayload, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

const TTL = ExpirationTime.fromMinutes(10)
const RUN = `r${Date.now().toString(36)}`

function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name} (see .env.example)`)
    process.exit(1)
  }
  return value
}

const batchN = Number(process.env.BATCH_N ?? "100")
const rpcUrl = env("RPC_URL").replace(/\/+$/, "")
const apiKey = env("API_KEY", false)

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

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("PRIVATE_KEY") as Hex,
})
console.log(
  `batch experiment: N=${batchN}, run ${RUN}, address ${writer.address}, chain ${writer.chainId}\n`,
)

let started = Date.now()
const { txHash: createTx, createdEntities } = await writer.mutateEntities({
  creates: Array.from({ length: batchN }, (_, i) => ({
    payload: jsonToPayload({ probe: "batch", i, run: RUN }),
    contentType: "application/json",
    attributes: { kind: str("batch-experiment"), run: str(RUN), rank: i32(i) },
    expires: TTL,
  })),
})
report(`create x${batchN}`, (Date.now() - started) / 1000, await txStats(createTx))
if (createdEntities.length !== batchN) {
  throw new Error(`expected ${batchN} keys, got ${createdEntities.length}`)
}

started = Date.now()
const { txHash: extendTx } = await writer.mutateEntities({
  extensions: createdEntities.map((entityKey) => ({
    entityKey,
    expires: ExpirationTime.fromMinutes(15),
  })),
})
report(`extend x${batchN}`, (Date.now() - started) / 1000, await txStats(extendTx))

started = Date.now()
const { txHash: patch1Tx } = await writer.mutateEntities({
  patches: createdEntities.map((entityKey) => ({
    entityKey,
    set: { status: str("patched") },
  })),
})
report(`patch x${batchN} (1 mutation)`, (Date.now() - started) / 1000, await txStats(patch1Tx))

started = Date.now()
const { txHash: patch3Tx } = await writer.mutateEntities({
  patches: createdEntities.map((entityKey, i) => ({
    entityKey,
    set: { status: str("repatched"), extra: str("x"), rank: i32(i + 1000) },
  })),
})
report(`patch x${batchN} (3 mutations)`, (Date.now() - started) / 1000, await txStats(patch3Tx))

started = Date.now()
const { txHash: deleteTx } = await writer.mutateEntities({
  deletes: createdEntities.map((entityKey) => ({ entityKey })),
})
report(`delete x${batchN}`, (Date.now() - started) / 1000, await txStats(deleteTx))

console.log("\nbatch experiment done")
