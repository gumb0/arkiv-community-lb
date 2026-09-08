// One-off probe: does the chain accept a permanent entity (expiry = u64 max),
// what does it cost, and is it still deletable? A second leg asks whether
// finite lifetimes are capped (the doc's MAX_LIFETIME): a create ten years
// out and an extend asking for one more year, each accepted or reverted.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header).
// Run: npm run permanent

import { ExpirationTime, jsonToPayload, MAX_EXPIRES_AT, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

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

async function rawRpc(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  })
  const body = (await res.json()) as { result?: unknown; error?: { message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message}`)
  return body.result
}

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("WRITER_PRIVATE_KEY") as Hex,
})
console.log(`permanent probe: address ${writer.address}, chain ${writer.chainId}`)
console.log(`MAX_EXPIRES_AT = ${MAX_EXPIRES_AT} (u64 max: ${MAX_EXPIRES_AT === 2n ** 64n - 1n})\n`)

const { entityKey, txHash, expiresAt } = await writer.createEntity({
  payload: jsonToPayload({ probe: "permanent" }),
  contentType: "application/json",
  attributes: { kind: str("permanent-probe") },
  expires: ExpirationTime.permanent(),
})
const receipt = (await rawRpc("eth_getTransactionReceipt", [txHash])) as { gasUsed: Hex }
console.log(`created ${entityKey}`)
console.log(`  SDK-reported expiresAt: ${expiresAt} (sentinel: ${expiresAt === MAX_EXPIRES_AT})`)
console.log(`  gasUsed: ${BigInt(receipt.gasUsed)} (finite-TTL create baseline: ~95,926 + 21,000 intrinsic)`)

const row = (
  (await rawRpc("arkiv_query", [
    `$key = key(${entityKey})`,
    { select: { key: true, expiresAt: true } },
  ])) as { data?: { expiresAt?: Hex }[] }
).data?.[0]
console.log(`  chain-reported $expiresAt: ${row?.expiresAt} (${BigInt(row?.expiresAt ?? "0x0")})`)

await writer.deleteEntity({ entityKey })
console.log(`deleted — a permanent entity is still owner-deletable`)

// Lifetime bound. Blocks, not seconds: the SDK converts durations at a
// fixed 2 s per block, and the point is how far the engine lets a finite
// expiry reach, whatever the block time.
const BLOCKS_PER_YEAR = (365 * 24 * 3600) / 2
const head = BigInt((await rawRpc("eth_blockNumber", [])) as Hex)

async function attempt(label: string, run: () => Promise<unknown>) {
  try {
    await run()
    console.log(`${label}: accepted`)
    return true
  } catch (error) {
    const chain: string[] = []
    for (let e: unknown = error; e instanceof Error; e = e.cause) chain.push(`${e.name}: ${e.message.split("\n")[0]}`)
    console.log(`${label}: reverted\n  ${chain.join("\n  ")}`)
    return false
  }
}

async function createFor(expires: Parameters<typeof writer.createEntity>[0]["expires"]) {
  const { entityKey } = await writer.createEntity({
    payload: jsonToPayload({ probe: "lifetime-bound" }),
    contentType: "application/json",
    attributes: { kind: str("permanent-probe") },
    expires,
  })
  return entityKey
}

console.log(`\nlifetime bound (head ${head}, ${BLOCKS_PER_YEAR} blocks per year):`)
let key: Hex | undefined
await attempt("create at head + 10 years", async () => {
  key = await createFor(ExpirationTime.atBlock(head + BigInt(10 * BLOCKS_PER_YEAR)))
})
if (key) await writer.deleteEntity({ entityKey: key })

// A short-lived entity, so the extend is a real extension and not a
// shortening the engine would refuse for that reason instead.
const short = await createFor(ExpirationTime.fromBlocks(100))
await attempt("extend a 100-block entity by 1 year (minLifetime)", () =>
  writer.extendEntity({ entityKey: short, expires: ExpirationTime.fromBlocks(BLOCKS_PER_YEAR) }),
)
await writer.deleteEntity({ entityKey: short })
