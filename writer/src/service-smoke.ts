// Smoke test for the chain-writer HTTP shell: starts the service on an
// ephemeral port, drives every route over HTTP, and verifies the results with
// raw JSON-RPC — the read path the Rust side will use.
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY (sent as Authorization: Bearer).
// Run: npm run service-smoke

import { ExpirationTime, jsonToPayload } from "@arkiv-network/sdk"
import { toHex, type Hex } from "viem"
import { startService } from "./service.ts"
import { createWriter } from "./writer.ts"

const RUN = `r${Date.now().toString(36)}`
const TTL = { seconds: 300 }

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
  contentType?: string
  attributes?: { name: string; type: string; value: unknown }[]
}

async function query(q: string): Promise<QueriedRow[]> {
  const result = (await rawRpc("arkiv_query", [
    q,
    { select: { key: true, payload: true, attributes: true, expiresAt: true, contentType: true } },
  ])) as { data?: QueriedRow[] }
  return result.data ?? []
}

const byKey = async (entityKey: Hex) => (await query(`$key = key(${entityKey})`))[0]

const attrValue = (row: QueriedRow | undefined, name: string) =>
  row?.attributes?.find((attribute) => attribute.name === name)?.value

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

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("PRIVATE_KEY") as Hex,
})
const service = await startService(writer, { port: 0 })
const base = `http://${service.host}:${service.port}`
console.log(`service on ${base}, address ${writer.address}, chain ${writer.chainId}, run ${RUN}\n`)

type Posted = { status: number; body: Record<string, unknown> }

async function post(route: string, body: unknown): Promise<Posted> {
  const res = await fetch(`${base}${route}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  })
  return { status: res.status, body: (await res.json()) as Record<string, unknown> }
}

async function ok(route: string, body: unknown): Promise<Record<string, unknown>> {
  const { status, body: result } = await post(route, body)
  assert(status === 200, `${route} returned 200 (got ${status}: ${JSON.stringify(result)})`)
  return result
}

const b64 = (bytes: Uint8Array) => Buffer.from(bytes).toString("base64")
const payload = jsonToPayload({ probe: "service-smoke", run: RUN })

const entityKey = await step("POST /create — typed attributes and base64 payload", async () => {
  const result = await ok("/create", {
    payload: b64(payload),
    contentType: "application/json",
    attributes: {
      kind: { type: "str", value: "service-smoke" },
      run: { type: "str", value: RUN },
      rank: { type: "i32", value: 1 },
      live: { type: "bool", value: true },
      big: { type: "u256", value: "123456789012345678901234567890" },
      owner: { type: "addr", value: writer.address },
      price: { type: "dec", value: "1.25" },
    },
    expires: TTL,
  })
  const key = result.entityKey as Hex
  console.log(`  key ${key}, expiresAt ${result.expiresAt} (bigint as string)`)
  assert(typeof result.expiresAt === "string", "bigints serialize as decimal strings")

  const row = await byKey(key)
  assert(row !== undefined, "entity readable over raw JSON-RPC")
  assert(row.payload === toHex(payload), "base64 payload round-trips byte-identical")
  assert(attrValue(row, "run") === RUN, "str attribute round-trips")
  assert(attrValue(row, "rank") === 1, "i32 attribute round-trips")
  assert(attrValue(row, "live") === true, "bool attribute round-trips")
  const types = (row.attributes ?? []).map((a) => `${a.name}:${a.type}=${a.value}`).join(" ")
  console.log(`  attributes ${types}`)
  return key
})

await step("POST /patch — set, unset, payload replacement", async () => {
  const replacement = jsonToPayload({ probe: "service-smoke", run: RUN, patched: true })
  await ok("/patch", {
    entityKey,
    set: { rank: { type: "i32", value: 2 }, status: { type: "str", value: "patched" } },
    unset: ["live"],
    payload: b64(replacement),
    contentType: "application/json",
  })
  const row = await byKey(entityKey)
  assert(attrValue(row, "rank") === 2, "set overwrote rank")
  assert(attrValue(row, "status") === "patched", "set added status")
  assert(attrValue(row, "live") === undefined, "unset removed live")
  assert(row?.payload === toHex(replacement), "payload replaced")
})

await step("POST /extend — expiry grows", async () => {
  const before = BigInt((await byKey(entityKey))?.expiresAt ?? "0x0")
  const result = await ok("/extend", { entityKey, expires: { seconds: 900 } })
  const after = BigInt((await byKey(entityKey))?.expiresAt ?? "0x0")
  console.log(`  expiresAt ${before} -> ${after} (service said ${result.expiresAt})`)
  assert(after > before, "expiry strictly increased")
})

const batched = await step("POST /mutate — two creates and a patch in one tx", async () => {
  const make = (n: number) => ({
    payload: b64(jsonToPayload({ probe: "service-batch", n, run: RUN })),
    contentType: "application/json",
    attributes: {
      kind: { type: "str", value: "service-smoke" },
      run: { type: "str", value: RUN },
      rank: { type: "i32", value: n },
    },
    expires: TTL,
  })
  const result = await ok("/mutate", {
    creates: [make(10), make(11)],
    patches: [{ entityKey, set: { batched: { type: "str", value: "yes" } } }],
  })
  const created = result.createdEntities as Hex[]
  assert(created.length === 2, "two keys returned, in batch order")
  assert((result.patchedEntities as Hex[]).length === 1, "one patched key returned")
  const [a, b, patched] = await Promise.all([byKey(created[0]), byKey(created[1]), byKey(entityKey)])
  assert(a !== undefined && b !== undefined, "both created entities readable")
  assert(attrValue(a, "rank") === 10 && attrValue(b, "rank") === 11, "batch order preserved")
  assert(attrValue(patched, "batched") === "yes", "patch applied in the same tx")
  console.log(`  one tx ${result.txHash}`)
  return created
})

const parallel = await step("parallel /create requests both land (the queue serializes)", async () => {
  const fire = (tag: string) =>
    ok("/create", {
      payload: b64(jsonToPayload({ probe: "service-parallel", tag, run: RUN })),
      contentType: "application/json",
      attributes: {
        kind: { type: "str", value: "service-smoke" },
        run: { type: "str", value: RUN },
        tag: { type: "str", value: tag },
      },
      expires: TTL,
    })
  const results = await Promise.all([fire("p1"), fire("p2")])
  const keys = results.map((result) => result.entityKey as Hex)
  assert(new Set(keys).size === 2, "two distinct keys")
  const rows = await Promise.all(keys.map(byKey))
  assert(rows.every((row) => row !== undefined), "both entities on chain — neither request lost")
  console.log(`  ${keys.map((k) => k.slice(0, 12) + "…").join(", ")} in separate transactions`)
  return keys
})

await step("bad request bodies are 400, not 500", async () => {
  const cases: [string, unknown][] = [
    ["/create", { payload: b64(payload), contentType: "application/json" }],
    ["/create", { payload: b64(payload), contentType: "application/json", expires: { years: 3 } }],
    ["/patch", { entityKey: "no-0x-prefix" }],
    ["/patch", { entityKey, set: { rank: { type: "int", value: 1 } } }],
    ["/extend", { entityKey, expires: "forever" }],
  ]
  for (const [route, body] of cases) {
    const { status, body: result } = await post(route, body)
    const chain = result.error as { name: string; message: string }[]
    assert(status === 400, `${route} ${JSON.stringify(body).slice(0, 60)} → 400 (got ${status})`)
    assert(Array.isArray(chain) && chain.length > 0, "error chain present")
    console.log(`  400 ${chain[0].message}`)
  }
})

await step("unknown route is 404", async () => {
  const { status } = await post("/nope", {})
  assert(status === 404, `unknown route → 404 (got ${status})`)
})

await step("a reverting write is 500 with the walked cause chain", async () => {
  const { status, body } = await post("/extend", { entityKey, expires: { seconds: 2 } })
  const chain = body.error as { name: string; message: string }[]
  assert(status === 500, `shrinking extend → 500 (got ${status})`)
  assert(chain[0].name === "EntityMutationError", "top of chain is the SDK's typed error")
  assert(chain.length > 1, "cause chain walked past the top error")
  console.log(`  ${chain.map((link) => link.name).join(" → ")}`)
})

await step("cleanup: one /mutate deletes everything this run created", async () => {
  const keys = [entityKey, ...batched, ...parallel]
  await ok("/mutate", { deletes: keys.map((key) => ({ entityKey: key })) })
  const left = await query(`kind = str('service-smoke') AND run = str('${RUN}')`)
  assert(left.length === 0, `no entities left (found ${left.length})`)
})

await service.close()
console.log("\nservice smoke: all legs passed")
