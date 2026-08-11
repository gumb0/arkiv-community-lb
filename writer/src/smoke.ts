// Smoke test for the writer module against a live Arkiv network.
// Reads stay raw JSON-RPC on purpose: they validate the read path the
// LB itself will use, not the SDK's reader.
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY (appended as a path segment).
// Run: npm run smoke

import { jsonToPayload } from "@arkiv-network/sdk"
import { toHex, type Hex } from "viem"
import { createWriter } from "./writer.ts"

const TTL_SECONDS = 30 // must be a multiple of the 2s block time

function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name} (see .env.example)`)
    process.exit(1)
  }
  return value
}

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

type QueriedEntity = {
  key: Hex
  value?: string
  expiresAt?: Hex
  creator?: Hex
  attributes?: { key: string; value: string }[]
}

async function queryByKey(entityKey: Hex): Promise<QueriedEntity | undefined> {
  const result = (await rawRpc("arkiv_query", [
    `$key = ${entityKey}`,
    {
      includeData: {
        key: true,
        payload: true,
        attributes: true,
        expiration: true,
        creator: true,
      },
    },
  ])) as { data: QueriedEntity[] }
  return result.data[0]
}

async function blockNumber(): Promise<bigint> {
  return BigInt((await rawRpc("eth_blockNumber", [])) as string)
}

function assert(cond: boolean, what: string): asserts cond {
  if (!cond) throw new Error(`assertion failed: ${what}`)
}

async function step<T>(name: string, run: () => Promise<T>): Promise<T> {
  const started = Date.now()
  try {
    const result = await run()
    console.log(`✓ ${name} (${((Date.now() - started) / 1000).toFixed(1)}s)`)
    return result
  } catch (error) {
    console.error(`✗ ${name}`)
    throw error
  }
}

const writer = await step("connect (chain id probed)", () =>
  createWriter({ rpcUrl, privateKey: env("PRIVATE_KEY") as Hex }),
)
console.log(`  address ${writer.address}, chain id ${writer.chainId}`)

await step("account is funded", async () => {
  const balance = BigInt((await rawRpc("eth_getBalance", [writer.address, "latest"])) as string)
  console.log(`  balance ${balance} wei`)
  assert(balance > 0n, "account has gas")
})

const payload = jsonToPayload({ probe: "arkiv-writer-smoke", startedAt: new Date().toISOString() })

const { entityKey } = await step("create entity", async () => {
  const created = await writer.createEntity({
    payload,
    contentType: "application/json",
    attributes: [
      { key: "kind", value: "smoke" },
      { key: "run", value: Date.now() },
    ],
    expiresIn: TTL_SECONDS,
  })
  console.log(`  key ${created.entityKey}`)
  return created
})

const firstExpiry = await step("raw read-back matches", async () => {
  const entity = await queryByKey(entityKey)
  assert(entity !== undefined, "entity found via arkiv_query")
  assert(entity.value === toHex(payload), "payload round-trips byte-identical")
  assert(
    entity.creator?.toLowerCase() === writer.address.toLowerCase(),
    "creator is the writing address",
  )
  assert(entity.expiresAt !== undefined, "expiration returned")
  return BigInt(entity.expiresAt)
})

await step("extend moves expiry forward", async () => {
  await writer.extendEntity({ entityKey, expiresIn: TTL_SECONDS })
  const entity = await queryByKey(entityKey)
  assert(entity !== undefined, "entity still present")
  const extended = BigInt(entity.expiresAt ?? "0x0")
  console.log(`  expiresAt ${firstExpiry} -> ${extended}`)
  assert(extended > firstExpiry, "expiry strictly increased")
  return extended
})

await step("batch: two creates in one transaction", async () => {
  const make = (n: number) => ({
    payload: jsonToPayload({ probe: "smoke-batch", n }),
    contentType: "application/json",
    attributes: [{ key: "kind", value: "smoke-batch" }],
    expiresIn: TTL_SECONDS,
  })
  const { txHash, createdEntities } = await writer.mutateEntities({
    creates: [make(1), make(2)],
  })
  assert(createdEntities.length === 2, "two entity keys returned")
  const [a, b] = await Promise.all([
    queryByKey(createdEntities[0]),
    queryByKey(createdEntities[1]),
  ])
  assert(a !== undefined && b !== undefined, "both entities readable")
  console.log(`  one tx ${txHash}, keys ${createdEntities.join(", ")}`)
  return createdEntities
})

await step("delete removes immediately", async () => {
  const victim = await writer.createEntity({
    payload: jsonToPayload({ probe: "smoke-delete" }),
    contentType: "application/json",
    attributes: [{ key: "kind", value: "smoke" }],
    expiresIn: TTL_SECONDS,
  })
  await writer.deleteEntity({ entityKey: victim.entityKey })
  const entity = await queryByKey(victim.entityKey)
  assert(entity === undefined, "deleted entity is gone from queries")
})

await step("entity expires on its own", async () => {
  const entity = await queryByKey(entityKey)
  assert(entity !== undefined, "entity present before expiry")
  const expiresAt = BigInt(entity.expiresAt ?? "0x0")
  const deadline = Date.now() + 5 * 60_000
  while ((await blockNumber()) <= expiresAt) {
    assert(Date.now() < deadline, "expiry block reached within 5 minutes")
    await new Promise((resolve) => setTimeout(resolve, 4000))
  }
  // Expiry enforcement may lag the expiry block; poll the query with a
  // grace window instead of asserting on the first read after crossing.
  const graceDeadline = Date.now() + 2 * 60_000
  while (Date.now() < graceDeadline) {
    if ((await queryByKey(entityKey)) === undefined) return
    await new Promise((resolve) => setTimeout(resolve, 4000))
  }
  assert(false, "expired entity left query results within 2 minutes of its expiry block")
})

console.log("\nsmoke: all legs passed")
