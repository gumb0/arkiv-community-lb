// Batch-scale tariff probe on the 0.8 API: N-op transactions, one phase
// per op kind — wall time, gas (total and per op), calldata size. The
// patch phases run twice (one vs three mutations) to separate per-op
// from per-mutation pricing. A last phase checks that a batch is
// all-or-nothing, which the design leans on and only the doc has claimed.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header);
// BATCH_N (default 50).
// Run: npm run batch
//
// The node caps raw transaction size at 128 KiB (txpool default), and the
// create phase reaches it at N=98 with these payloads — the error is
// "oversized data", and a gateway in front may reject the same batch earlier
// as HTTP 413. The default leaves room for payloads to grow; raise it to
// find the ceiling again.

import { ExpirationTime, i32, jsonToPayload, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

const TTL = ExpirationTime.fromBlocks(300) // blocks, so a faster chain means the same thing
const RUN = `r${Date.now().toString(36)}`

function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name} (see .env.example)`)
    process.exit(1)
  }
  return value
}

const batchN = Number(process.env.BATCH_N ?? "50")
const rpcUrl = env("ARKIV_RPC_URL").replace(/\/+$/, "")
const apiKey = env("ARKIV_API_KEY", false)

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
  privateKey: env("WRITER_PRIVATE_KEY") as Hex,
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
    expires: ExpirationTime.fromBlocks(450),
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

// One doomed op must take the whole batch with it. The extend names a key
// nothing can own, which the SDK cannot know is wrong, so the engine is the
// one deciding — unlike a bad expiry, which never leaves the client.
const doomedKey = `0x${"ee".repeat(32)}` as Hex
let reverted = false
try {
  await writer.mutateEntities({
    creates: [
      {
        payload: jsonToPayload({ probe: "atomicity", run: RUN }),
        contentType: "application/json",
        attributes: { kind: str("batch-atomicity"), run: str(RUN) },
        expires: TTL,
      },
    ],
    extensions: [{ entityKey: doomedKey, expires: ExpirationTime.fromBlocks(450) }],
  })
} catch (error) {
  reverted = true
  const cause = (error as Error).cause as Error | undefined
  console.log(
    `\natomicity: batch rejected — ${(error as Error).constructor.name}` +
      `${cause ? ` → ${cause.constructor.name}` : ""}`,
  )
}

const survivors = (
  (await rawRpc("arkiv_query", [
    `kind = str('batch-atomicity') AND run = str('${RUN}')`,
    { select: { key: true } },
  ])) as { data?: { key: Hex }[] }
).data ?? []
console.log(`atomicity: valid create in that batch landed: ${survivors.length} (expected 0)`)

if (survivors.length > 0) {
  await writer.mutateEntities({ deletes: survivors.map(({ key }) => ({ entityKey: key })) })
}
if (!reverted || survivors.length > 0) {
  throw new Error("a batch applied part of itself: execute is not all-or-nothing here")
}

console.log("\nbatch experiment done")
