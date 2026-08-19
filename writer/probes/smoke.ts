// Smoke test for the writer module against a live Arkiv network (0.8 API).
// Reads stay raw JSON-RPC on purpose: they validate the read path the
// LB itself will use, not the SDK's reader.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header).
// Run: npm run smoke

import { ExpirationTime, i32, jsonToPayload, str } from "@arkiv-network/sdk"
import { toHex, type Hex } from "viem"
import { createWriter } from "../src/writer.ts"

const TTL = ExpirationTime.fromSeconds(60)
const RUN = `r${Date.now().toString(36)}`

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

type QueriedRow = {
  key?: Hex
  payload?: Hex
  expiresAt?: Hex
  creator?: Hex
  contentType?: string
  attributes?: { name: string; type: string; value: unknown }[]
}

const SELECT_ALL = {
  key: true,
  payload: true,
  attributes: true,
  expiresAt: true,
  creator: true,
  contentType: true,
}

async function query(q: string, options: Record<string, unknown> = {}): Promise<QueriedRow[]> {
  const result = (await rawRpc("arkiv_query", [q, { select: SELECT_ALL, ...options }])) as {
    data?: QueriedRow[]
  }
  return result.data ?? []
}

const byKey = async (entityKey: Hex) => (await query(`$key = key(${entityKey})`))[0]

async function blockNumber(): Promise<bigint> {
  return BigInt((await rawRpc("eth_blockNumber", [])) as string)
}

function assert(cond: boolean, what: string): asserts cond {
  if (!cond) throw new Error(`assertion failed: ${what}`)
}

/** An indexed read that must have found something — asserts, then narrows. */
function required<T>(value: T | undefined, what: string): T {
  assert(value !== undefined, `${what} is present`)
  return value
}

const attrValue = (row: QueriedRow | undefined, name: string) =>
  row?.attributes?.find((attribute) => attribute.name === name)?.value

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
  createWriter({ rpcUrl, apiKey: apiKey || undefined, privateKey: env("WRITER_PRIVATE_KEY") as Hex }),
)
console.log(`  address ${writer.address}, chain id ${writer.chainId}, run ${RUN}`)

await step("account is funded", async () => {
  const balance = BigInt((await rawRpc("eth_getBalance", [writer.address, "latest"])) as string)
  console.log(`  balance ${balance} wei`)
  assert(balance > 0n, "account has gas")
})

const payload = jsonToPayload({ probe: "arkiv-writer-smoke", run: RUN })

const { entityKey } = await step("create entity", async () => {
  const created = await writer.createEntity({
    payload,
    contentType: "application/json",
    attributes: { kind: str("smoke"), run: str(RUN), rank: i32(1), flag: str("y") },
    expires: TTL,
  })
  console.log(`  key ${created.entityKey}, expiresAt ${created.expiresAt}`)
  return created
})

await step("raw read-back matches (new grammar + select)", async () => {
  const row = await byKey(entityKey)
  assert(row !== undefined, "entity found via arkiv_query")
  assert(row.payload === toHex(payload), "payload round-trips byte-identical")
  assert(row.creator?.toLowerCase() === writer.address.toLowerCase(), "creator is the writer")
  assert(row.contentType === "application/json", "contentType round-trips")
  assert(attrValue(row, "rank") === 1, "i32 attribute comes back as JSON number")
  assert(attrValue(row, "run") === RUN, "str attribute round-trips")
})

await step("patch: set + tombstone unset, expiry untouched", async () => {
  const before = await byKey(entityKey)
  await writer.patchEntity({
    entityKey,
    set: { rank: i32(2), status: str("patched") },
    unset: ["flag"],
  })
  const after = await byKey(entityKey)
  assert(after !== undefined, "entity still present")
  assert(attrValue(after, "rank") === 2, "set overwrote rank")
  assert(attrValue(after, "status") === "patched", "set added status")
  assert(attrValue(after, "flag") === undefined, "unset removed flag (tombstone)")
  assert(after.payload === toHex(payload), "payload untouched by attribute patch")
  assert(after.expiresAt === before?.expiresAt, "patch never moves expiry")
})

await step("extend: grows, and shrinking reverts with a cause", async () => {
  const before = BigInt((await byKey(entityKey))?.expiresAt ?? "0x0")
  await writer.extendEntity({ entityKey, expires: ExpirationTime.fromMinutes(5) })
  const grown = BigInt((await byKey(entityKey))?.expiresAt ?? "0x0")
  console.log(`  expiresAt ${before} -> ${grown}`)
  assert(grown > before, "expiry strictly increased")
  try {
    await writer.extendEntity({ entityKey, expires: ExpirationTime.fromSeconds(2) })
    assert(false, "shrinking extend should revert")
  } catch (error) {
    assert((error as Error).name === "EntityMutationError", "typed EntityMutationError")
    assert((error as Error).cause !== undefined, "cause is chained (#90 fixed)")
  }
})

await step("readonly: patching a readonly entity reverts", async () => {
  const ro = await writer.createEntity({
    payload: jsonToPayload({ probe: "smoke-readonly", run: RUN }),
    contentType: "application/json",
    attributes: { kind: str("smoke"), run: str(RUN) },
    expires: TTL,
    flags: { readonly: true },
  })
  try {
    await writer.patchEntity({ entityKey: ro.entityKey, set: { rank: i32(9) } })
    assert(false, "patching readonly should revert")
  } catch (error) {
    assert((error as Error).name === "EntityMutationError", "typed EntityMutationError")
  }
})

await step("batch: two creates + one patch in one transaction", async () => {
  const make = (n: number) => ({
    payload: jsonToPayload({ probe: "smoke-batch", n, run: RUN }),
    contentType: "application/json",
    attributes: { kind: str("smoke-batch"), run: str(RUN), rank: i32(n) },
    expires: TTL,
  })
  const { txHash, createdEntities } = await writer.mutateEntities({
    creates: [make(1), make(2)],
    patches: [{ entityKey, set: { batched: str("yes") } }],
  })
  assert(createdEntities.length === 2, "two entity keys returned")
  const [a, b, patched] = await Promise.all([
    byKey(required(createdEntities[0], "first created key")),
    byKey(required(createdEntities[1], "second created key")),
    byKey(entityKey),
  ])
  assert(a !== undefined && b !== undefined, "both created entities readable")
  assert(attrValue(patched, "batched") === "yes", "patch applied in the same tx")
  console.log(`  one tx ${txHash}`)
})

await step("delete removes immediately", async () => {
  const victim = await writer.createEntity({
    payload: jsonToPayload({ probe: "smoke-delete", run: RUN }),
    contentType: "application/json",
    attributes: { kind: str("smoke"), run: str(RUN) },
    expires: TTL,
  })
  await writer.deleteEntity({ entityKey: victim.entityKey })
  assert((await byKey(victim.entityKey)) === undefined, "deleted entity gone from queries")
})

await step("expiry: entity leaves queries; the $expiresAt filter is exact at the boundary", async () => {
  const short = await writer.createEntity({
    payload: jsonToPayload({ probe: "smoke-expiry", run: RUN }),
    contentType: "application/json",
    attributes: { kind: str("smoke-expiry"), run: str(RUN) },
    expires: ExpirationTime.fromSeconds(10),
  })
  const expiresAt = short.expiresAt
  const deadline = Date.now() + 2 * 60_000
  while ((await blockNumber()) <= expiresAt) {
    assert(Date.now() < deadline, "expiry block reached within 2 minutes")
    await new Promise((resolve) => setTimeout(resolve, 2000))
  }
  const head = await blockNumber()
  const unfiltered = await query(`kind = str('smoke-expiry') AND run = str('${RUN}')`)
  const filtered = await query(
    `kind = str('smoke-expiry') AND run = str('${RUN}') AND $expiresAt > u64(${head})`,
  )
  console.log(`  unfiltered: ${unfiltered.length} row(s), $expiresAt-filtered: ${filtered.length}`)
  assert(filtered.length === 0, "the $expiresAt filter excludes the expired entity")
  // The unfiltered count is printed, not asserted: cheesecake drops expired
  // rows promptly (0 expected), but how fast is the node's business — the
  // filter is what we rely on at the boundary.
})

console.log("\nsmoke: all legs passed")
