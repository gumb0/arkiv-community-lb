// Lifetime probe: the chain caps an entity's lifetime. A permanent
// expiry (u64 max) is refused, and the refusal names the cap. This
// probe reads the cap from that refusal and checks it: a create at the
// cap is accepted, one block more is refused, and an extend past it is
// refused. The cap is set in the genesis, so it can differ per network.
// Settle's receipts are written for exactly this long.
//
// Env: WRITER_PRIVATE_KEY_FILE, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header).
// Run: npm run permanent

import {
  bytes,
  defineSchema,
  ExpirationTime,
  type Expiry,
  MAX_EXPIRES_AT,
  str,
} from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"
import { env, privateKeyFromFile } from "../src/env.ts"

const rpcUrl = env("ARKIV_RPC_URL").replace(/\/+$/, "")
const apiKey = env("ARKIV_API_KEY", false)

// A JSON document in a bytes attribute, the way the LB stores its records.
const document = (value: unknown) => bytes(new TextEncoder().encode(JSON.stringify(value)))

const Probe = defineSchema({ kind: str().indexed(), payload: bytes() })

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

function assert(cond: boolean, what: string): asserts cond {
  if (!cond) throw new Error(`assertion failed: ${what}`)
}

/** The error chain's messages, first lines, top to bottom. */
function reasons(error: unknown): string {
  const chain: string[] = []
  for (let e: unknown = error; e instanceof Error; e = e.cause) chain.push(e.message.split("\n")[0] ?? "")
  return chain.join(" | ")
}

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: privateKeyFromFile(),
})
console.log(`lifetime probe: address ${writer.address}, chain ${writer.chainId}\n`)

async function createFor(expiration: Expiry): Promise<Hex> {
  const { entityKey } = await writer.createEntity({
    schema: Probe,
    values: { kind: str("permanent-probe"), payload: document({ probe: "lifetime" }) },
    expiration,
  })
  return entityKey
}

/** Runs a write expected to be refused; returns the refusal's reasons. */
async function refused(label: string, run: () => Promise<unknown>): Promise<string> {
  try {
    await run()
  } catch (error) {
    const why = reasons(error)
    console.log(`${label}: refused\n  ${why}`)
    return why
  }
  throw new Error(`${label}: accepted, expected a refusal`)
}

// The SDK's largest expiry, u64 max, is refused, and the refusal says
// what the cap is.
const permanent = await refused("create with expiry u64 max (permanent)", () =>
  createFor(ExpirationTime.atBlock(MAX_EXPIRES_AT)),
)
const named = /maximum lifetime of (\d+) blocks/.exec(permanent)
assert(named !== null, "the refusal names the chain's maximum lifetime")
const cap = BigInt(named[1] ?? "0")
console.log(`\nthe chain's maximum lifetime: ${cap} blocks (${(Number(cap) * 2) / 86400} days at 2 s)\n`)

// The edge: exactly the cap is accepted, one more is refused. A create
// lands a block or two after the head is read, so the ask is in blocks
// from the landing block, which the SDK's fromBlocks resolves on chain.
const atCap = await createFor(ExpirationTime.fromBlocks(Number(cap)))
console.log(`create for exactly ${cap} blocks: accepted (${atCap})`)
const overCap = await refused(`create for ${cap + 1n} blocks`, () =>
  createFor(ExpirationTime.fromBlocks(Number(cap) + 1)),
)
assert(overCap.includes("maximum lifetime"), "one block over the cap is refused for the cap")

// An extend is bound the same way: a short-lived entity cannot be
// extended past the cap from the current block.
const short = await createFor(ExpirationTime.fromBlocks(100))
const extended = await refused(`extend a 100-block entity to ${cap + 1n} blocks`, () =>
  writer.extendEntity({ entityKey: short, expiration: ExpirationTime.fromBlocks(Number(cap) + 1) }),
)
assert(extended.includes("maximum lifetime"), "an extend past the cap is refused for the cap")
await writer.extendEntity({ entityKey: short, expiration: ExpirationTime.fromBlocks(Number(cap)) })
console.log(`extend the same entity to exactly ${cap} blocks: accepted`)

// What a lifetime costs: the cap against a day, on the same record.
const gasOf = async (txHash: Hex) =>
  BigInt(((await rawRpc("eth_getTransactionReceipt", [txHash])) as { gasUsed: Hex }).gasUsed)
const day = await writer.createEntity({
  schema: Probe,
  values: { kind: str("permanent-probe"), payload: document({ probe: "lifetime" }) },
  expiration: ExpirationTime.fromDays(1),
})
const capped = await writer.createEntity({
  schema: Probe,
  values: { kind: str("permanent-probe"), payload: document({ probe: "lifetime" }) },
  expiration: ExpirationTime.fromBlocks(Number(cap)),
})
console.log(
  `\ngas: a one-day create ${await gasOf(day.txHash)}, the same create at the cap ${await gasOf(capped.txHash)}`,
)

await writer.executeBatch({
  deletes: [atCap, short, day.entityKey, capped.entityKey].map((entityKey) => ({ entityKey })),
})
console.log("\nlifetime probe done")
