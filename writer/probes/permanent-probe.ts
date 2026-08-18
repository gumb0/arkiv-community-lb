// One-off probe: does the chain accept a permanent entity (expiry = u64 max),
// what does it cost, and is it still deletable?
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY (sent as Authorization: Bearer).
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

const rpcUrl = env("RPC_URL").replace(/\/+$/, "")
const apiKey = env("API_KEY", false)

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
  privateKey: env("PRIVATE_KEY") as Hex,
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
