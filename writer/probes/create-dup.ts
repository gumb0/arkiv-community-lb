// Create-duplication probe: where do the phantom entities come from?
// The page-budget probe showed 60 creates yielding ~110+ entities — extras
// carrying our own tag under keys the SDK never returned. The SDK reads
// returned keys from the receipt's EntityCreated log (count-asserted) and
// never resubmits, so this probe discriminates the remaining suspects by
// accounting for every transaction and every entity:
//   - each create's txHash + receipt block,
//   - every block's transactions over the run's range (a tx hash appearing
//     in two blocks = double inclusion; an unknown tx from our address =
//     client sent more than it thinks),
//   - every row the run-scoped query returns, with $createdAt, matched by
//     rank against the create that claims it.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY;
// DUP_FIXTURES (default 60), DUP_PAYLOAD_KB (default 100).
// Run: npm run create-dup

import { ExpirationTime, i32, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

const TTL = ExpirationTime.fromBlocks(600)
const RUN = `r${Date.now().toString(36)}`
const FIXTURES = Number(process.env.DUP_FIXTURES ?? "60")
const PAYLOAD_KB = Number(process.env.DUP_PAYLOAD_KB ?? "100")

function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name} (see .env.example)`)
    process.exit(1)
  }
  return value
}

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

const hex = (n: bigint) => `0x${n.toString(16)}`

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("WRITER_PRIVATE_KEY") as Hex,
})
const me = writer.address.toLowerCase()
console.log(
  `create-dup probe: ${FIXTURES} x ${PAYLOAD_KB} KB, run ${RUN}, address ${writer.address}\n`,
)

const startBlock = BigInt((await rawRpc("eth_blockNumber", [])) as string)
const startNonce = BigInt(
  (await rawRpc("eth_getTransactionCount", [writer.address, "latest"])) as string,
)

// ---- create, recording everything the client knows -------------------------
type Created = { rank: number; entityKey: Hex; txHash: Hex; receiptBlock: bigint }
const payload = new Uint8Array(PAYLOAD_KB * 1024).fill(0x61)
const created: Created[] = []
for (let i = 0; i < FIXTURES; i++) {
  const { entityKey, txHash } = await writer.createEntity({
    payload,
    contentType: "application/octet-stream",
    attributes: { kind: str("create-dup"), run: str(RUN), rank: i32(i) },
    expires: TTL,
  })
  const receipt = (await rawRpc("eth_getTransactionReceipt", [txHash])) as {
    blockNumber: Hex
  }
  created.push({ rank: i, entityKey, txHash, receiptBlock: BigInt(receipt.blockNumber) })
  if ((i + 1) % 20 === 0) console.log(`  ${i + 1}/${FIXTURES} created`)
}
const endBlock = BigInt((await rawRpc("eth_blockNumber", [])) as string)
const endNonce = BigInt(
  (await rawRpc("eth_getTransactionCount", [writer.address, "latest"])) as string,
)

// ---- account for every transaction in the range ----------------------------
const oursByHash = new Map(created.map((c) => [c.txHash.toLowerCase(), c]))
const inclusions = new Map<string, bigint[]>() // txHash -> blocks it appears in
let fromUs = 0
const unknownFromUs: string[] = []
for (let b = startBlock + 1n; b <= endBlock; b++) {
  const block = (await rawRpc("eth_getBlockByNumber", [hex(b), true])) as {
    transactions: { hash: Hex; from: Hex }[]
  }
  for (const tx of block.transactions ?? []) {
    const h = tx.hash.toLowerCase()
    if (tx.from.toLowerCase() !== me) continue
    fromUs++
    inclusions.set(h, [...(inclusions.get(h) ?? []), b])
    if (!oursByHash.has(h)) unknownFromUs.push(h)
  }
}
const doubleIncluded = [...inclusions.entries()].filter(([, blocks]) => blocks.length > 1)

console.log(`\ntransaction accounting (blocks ${startBlock + 1n}..${endBlock}):`)
console.log(`  account nonce advanced: ${endNonce - startNonce} (sent: ${FIXTURES})`)
console.log(`  txs from us seen in blocks: ${fromUs}`)
console.log(`  tx hashes included in >1 block: ${doubleIncluded.length}`)
for (const [h, blocks] of doubleIncluded.slice(0, 3)) {
  console.log(`    e.g. ${h} in blocks ${blocks.join(", ")}`)
}
console.log(`  txs from us the client never sent: ${unknownFromUs.length}`)

// ---- account for every entity the query returns ----------------------------
type Row = { key: Hex; createdAt?: Hex; attributes?: { name: string; value: unknown }[] }
const page = (await rawRpc("arkiv_query", [
  `kind = str('create-dup') AND run = str('${RUN}')`,
  {
    select: { key: true, createdAt: true, attributes: { rank: true } },
    limit: "0xc8",
  },
])) as { data?: Row[] }
const rows = page.data ?? []
const ours = new Set(created.map((c) => c.entityKey.toLowerCase()))
const foreign = rows.filter((row) => !ours.has(row.key.toLowerCase()))

console.log(`\nentity accounting (run-scoped query):`)
console.log(`  rows: ${rows.length} for ${FIXTURES} creates; not ours: ${foreign.length}`)
for (const row of foreign.slice(0, 5)) {
  const rank = row.attributes?.find((a) => a.name === "rank")?.value as number | undefined
  const twin = created.find((c) => c.rank === rank)
  const at = row.createdAt !== undefined ? BigInt(row.createdAt) : undefined
  const twinBlocks = twin ? inclusions.get(twin.txHash.toLowerCase()) : undefined
  console.log(
    `  foreign rank=${rank}: createdAt=${at} | its rank's tx receipt block=` +
      `${twin?.receiptBlock} inclusions=[${twinBlocks?.join(", ")}] ` +
      `${at !== undefined && twinBlocks?.includes(at) ? "<= MATCHES a second inclusion" : ""}`,
  )
}

// ---- cleanup: everything the query can see ---------------------------------
const allKeys = [...new Set(rows.map((r) => r.key))]
if (allKeys.length > 0) {
  await writer.executeBatch({ deletes: allKeys.map((entityKey) => ({ entityKey })) })
}
const left = (await rawRpc("arkiv_query", [
  `kind = str('create-dup')`,
  { select: { key: true }, limit: "0xc8" },
])) as { data?: unknown[] }
console.log(
  `\ncleanup: deleted ${allKeys.length}; still visible (any run): ${left.data?.length ?? 0}`,
)

console.log(
  `\nverdict: ` +
    (doubleIncluded.length > 0
      ? "DOUBLE INCLUSION — the node executed the same tx in multiple blocks"
      : endNonce - startNonce > BigInt(FIXTURES) || unknownFromUs.length > 0
        ? "CLIENT SENT EXTRA TRANSACTIONS"
        : foreign.length > 0
          ? "PHANTOMS WITHOUT EXTRA TXS — node-side entity or query fabrication"
          : "clean run — no anomaly this time"),
)
