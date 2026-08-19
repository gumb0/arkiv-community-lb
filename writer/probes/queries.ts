// Query-language probe against the 0.8 API: result semantics that only
// real entities can prove. Parse acceptance and error codes were covered
// by the raw curl battery; the smoke covers basic reads — this file checks
// what queries *return*: negation semantics, typed comparisons, system
// filters, projections, pagination.
//
// Fixtures are tagged with a unique run id and every probe is scoped to it;
// cleanup runs in a finally block.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header).
// Run: npm run queries

import { dec, ExpirationTime, i32, jsonToPayload, key, str, u256 } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

const FIXTURES = 30
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
  if (body.error) throw new Error(body.error.message)
  return body.result
}

type QueryRow = {
  key?: Hex
  attributes?: { name: string; type: string; value: unknown }[]
  attributeSchema?: { name: string; type: string }[]
}
type QueryResult = { data?: QueryRow[]; cursor?: string; blockNumber?: string }

async function query(q: string, options: Record<string, unknown> = {}): Promise<QueryResult> {
  return (await rawRpc("arkiv_query", [
    q,
    { select: { key: true, attributes: true }, ...options },
  ])) as QueryResult
}

/** Scoped to this run: other runs' fixtures can never be counted. */
const mine = (rest?: string) => `run = str('${RUN}')${rest ? ` AND ${rest}` : ""}`

async function probe(name: string, q: string, expected: number, options?: Record<string, unknown>) {
  try {
    const result = await query(q, options)
    const rows = result.data?.length ?? -1
    console.log(`${rows === expected ? "✓" : `✗ expected ${expected},`} ${name}: ${rows} rows`)
    return result
  } catch (error) {
    console.log(`✗ ${name}: ${(error as Error).message}`)
    return undefined
  }
}

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("WRITER_PRIVATE_KEY") as Hex,
})
console.log(`query probe: run ${RUN}, address ${writer.address}, chain ${writer.chainId}\n`)

const preFixtureBlock = BigInt((await rawRpc("eth_blockNumber", [])) as string)

// i = 0..29. Evens: group a, flag set, name aa-i. Odds: group b, no flag, name ab-i.
const { createdEntities: fixtures } = await writer.mutateEntities({
  creates: Array.from({ length: FIXTURES }, (_, i) => ({
    payload: jsonToPayload({ probe: "queries", i }),
    contentType: "application/json",
    attributes: {
      kind: str("query-probe"),
      run: str(RUN),
      group: str(i % 2 === 0 ? "a" : "b"),
      name: str(`${i % 2 === 0 ? "aa" : "ab"}-${i}`),
      rank: i32(i),
      big: u256(BigInt(1_000_000 + i)),
      score: dec(`${i}.5`),
      ...(i % 2 === 0 ? { flag: str("y") } : {}),
    },
    expires: TTL,
  })),
})
console.log(`fixtures: ${fixtures.length} created`)

// A key-typed attribute needs a real key, so it rides a follow-up patch.
const [first, second] = fixtures
if (first === undefined || second === undefined) throw new Error("fixtures were not created")
await writer.patchEntity({ entityKey: second, set: { ref: key(first) } })
console.log(`ref attribute patched onto fixture 1\n`)

const me = writer.address
const nobody = "0x1111111111111111111111111111111111111111"

try {
  console.log("--- typed comparisons ---")
  await probe("str equality", mine(), 30)
  await probe("AND conjunction", mine(`group = str('a')`), 15)
  await probe("i32 >", mine("rank > i32(19)"), 10)
  await probe("i32 untagged equivalent", mine("rank > 19"), 10)
  await probe("i32 parenthesized range", mine("(rank >= i32(10) AND rank < i32(20))"), 10)
  await probe("OR + parens", mine(`(group = str('a') OR rank > i32(25))`), 17)
  await probe("u256 range", mine("big > u256(1000019)"), 10)
  await probe("dec range", mine("score >= dec(10.5)"), 20)
  await probe("STARTSWITH prefix", mine("name STARTSWITH str('aa')"), 15)
  await probe("STARTSWITH shared prefix", mine("name STARTSWITH str('a')"), 30)
  await probe("key-typed attribute", mine(`ref = key(${fixtures[0]})`), 1)

  console.log("--- negation: NOT is the only form; complement includes absence ---")
  // The API doc specs both != (typed value-negation) and NOT (complement);
  // the implementation removed != with a directive error. Assert that.
  try {
    await query(mine(`group != str('a')`))
    console.log("✗ != rejected: it was accepted — implementation changed, re-check the doc")
  } catch (error) {
    const gone = (error as Error).message.includes("not part of the query language")
    console.log(`${gone ? "✓" : "✗"} != rejected with a directive error`)
  }
  await probe("NOT() complement includes absent-attribute rows",
    mine(`NOT (flag = str('y'))`), 15)

  console.log("--- system filters ---")
  await probe("creator filter", mine(`$creator = addr(${me})`), 30)
  await probe("owner filter", mine(`$owner = addr(${me})`), 30)
  await probe("creator excludes others", mine(`$creator = addr(${nobody})`), 0)
  await probe("wrong-case attr name matches nothing", mine(`Group = str('a')`), 0)

  console.log("--- options ---")
  await probe("atBlock before this run", mine(), 0, {
    atBlock: `0x${preFixtureBlock.toString(16)}`,
  })
  const schemaOnly = await probe("select attributeSchema only", mine(), 30, {
    select: { key: true, attributeSchema: true },
  })
  if (schemaOnly?.data?.[0]) {
    const row = schemaOnly.data[0]
    console.log(
      `  schema names: ${(row.attributeSchema ?? []).map((a) => a.name).join(",") || "MISSING"}; values included: ${row.attributes !== undefined}`,
    )
  }
  const subset = await probe("select attribute subset {rank}", mine(), 30, {
    select: { key: true, attributes: { rank: true } },
  })
  if (subset?.data?.[0]) {
    console.log(
      `  subset row attributes: ${(subset.data[0].attributes ?? []).map((a) => a.name).join(",")}`,
    )
  }
  // orderBy is removed from the options (closed set: atBlock, select,
  // limit, cursor). Assert the rejection so a quiet reintroduction shouts.
  try {
    await query(mine(), { orderBy: [{ name: "rank", type: "numeric", desc: true }] })
    console.log("✗ orderBy rejected: it was accepted — options set changed, re-check the doc")
  } catch (error) {
    const gone = (error as Error).message.includes("unknown field")
    console.log(`${gone ? "✓" : "✗"} orderBy rejected (closed options set)`)
  }

  console.log("--- pagination ---")
  const seen = new Set<string>()
  let cursor: string | undefined
  let pages = 0
  do {
    const result = await query(mine(), {
      select: { key: true },
      limit: "0xa",
      ...(cursor ? { cursor } : {}),
    })
    pages++
    for (const row of result.data ?? []) if (row.key) seen.add(row.key)
    cursor = result.cursor
  } while (cursor && pages < 10)
  const ok = seen.size === FIXTURES
  console.log(`${ok ? "✓" : "✗"} pagination: ${pages} pages, ${seen.size} distinct keys`)
} finally {
  await writer.mutateEntities({ deletes: fixtures.map((entityKey) => ({ entityKey })) })
  console.log(`\nfixtures deleted; query probe done`)
}
