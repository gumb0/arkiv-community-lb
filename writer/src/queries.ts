// Query-language probe: the raw arkiv_query sentences a non-SDK reader
// hand-builds. Creates a fixture set tagged with a unique run id, runs
// named probes against expected counts, prints an evidence table, deletes
// the fixtures.
//
// Every probe is scoped to this run's id: expired entities from earlier
// runs are still served by the chain (known bug), so an unscoped query
// silently counts other runs' fixtures too.
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY.
// Run: npm run queries

import { jsonToPayload } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "./writer.ts"

const FIXTURES = 30
const TTL_SECONDS = 600
const RUN = `r${Date.now().toString(36)}`

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
  if (body.error) throw new Error(body.error.message)
  return body.result
}

type QueryOptions = Record<string, unknown>
type QueryRow = { key?: Hex; expiresAt?: Hex; attributes?: { key: string; value: string }[] }
type QueryResult = { data?: QueryRow[] } & Record<string, unknown>

async function query(q: string, options: QueryOptions = {}): Promise<QueryResult> {
  return (await rawRpc("arkiv_query", [
    q,
    { includeData: { key: true, attributes: true, expiration: true }, ...options },
  ])) as QueryResult
}

/** Scoped to this run: leftovers from other runs can never be counted. */
const mine = (rest?: string) => (rest ? `run = "${RUN}" && ${rest}` : `run = "${RUN}"`)

async function probe(name: string, q: string, expected: number, options: QueryOptions = {}) {
  try {
    const result = await query(q, options)
    const rows = result.data?.length ?? -1
    const verdict = rows === expected ? "✓" : `✗ expected ${expected},`
    console.log(`${verdict} ${name}: ${rows} rows`)
    return result
  } catch (error) {
    console.log(`✗ ${name}: ${(error as Error).message}`)
    return undefined
  }
}

const writer = await createWriter({ rpcUrl, privateKey: env("PRIVATE_KEY") as Hex })
console.log(`query probe: run ${RUN}, address ${writer.address}, chain ${writer.chainId}\n`)

const preFixtureBlock = BigInt((await rawRpc("eth_blockNumber", [])) as string)

// group alternates a/b; rank = index; group-a rows also carry "flag".
const { createdEntities: fixtures } = await writer.mutateEntities({
  creates: Array.from({ length: FIXTURES }, (_, i) => ({
    payload: jsonToPayload({ probe: "queries", i }),
    contentType: "application/json",
    attributes: [
      { key: "kind", value: "query-probe" },
      { key: "run", value: RUN },
      { key: "group", value: i % 2 === 0 ? "a" : "b" },
      { key: "rank", value: i },
      ...(i % 2 === 0 ? [{ key: "flag", value: "y" }] : []),
    ],
    expiresIn: TTL_SECONDS,
  })),
})
console.log(`fixtures: ${fixtures.length} created\n`)

const me = writer.address
const nobody = `0x${"11".repeat(20)}`

try {
  // --- the sentences Rust will build ---
  await probe("string equality", mine(), 30)
  await probe("conjunction", mine(`group = "a"`), 15)
  await probe("numeric >", mine("rank > 19"), 10)
  await probe("numeric range", mine("(rank >= 10 && rank < 20)"), 10)
  await probe("disjunction", mine(`(group = "a" || rank > 25)`), 17)
  await probe("inequality !=", mine(`group != "a"`), 15)
  await probe("bare !flag (expect: parse error)", mine("!flag"), 15)
  await probe("NOT over a present attribute !(group = \"a\")", mine(`!(group = "a")`), 15)
  // group-b rows carry no "flag" attribute at all: 15 rows means negation
  // matches absent attributes, 0 means it only matches present-but-different.
  await probe("NOT as absence !(flag = \"y\")", mine(`!(flag = "y")`), 15)
  await probe("creator filter", mine(`$creator=${me}`), 30)
  await probe("owner filter", mine(`$owner=${me}`), 30)
  await probe("creator excludes others", mine(`$creator=${nobody}`), 0)

  // --- options ---
  const ordered = await probe("orderBy rank desc", mine(), 30, {
    orderBy: [{ name: "rank", type: "numeric", desc: true }],
  })
  if (ordered?.data) {
    const ranks = ordered.data.map((row) =>
      Number(row.attributes?.find((attribute) => attribute.key === "rank")?.value ?? -1),
    )
    const sorted = ranks.every((rank, i) => i === 0 || ranks[i - 1] >= rank)
    console.log(`  orderBy honoured: ${sorted} (first ranks: ${ranks.slice(0, 5).join(",")})`)
  }

  await probe("atBlock before this run", mine(), 0, {
    atBlock: `0x${preFixtureBlock.toString(16)}`,
  })

  // --- pagination ---
  const seen = new Set<string>()
  let cursor: string | undefined
  let pages = 0
  try {
    do {
      const result = await query(mine(), { resultsPerPage: "0xa", ...(cursor ? { cursor } : {}) })
      pages++
      for (const row of result.data ?? []) if (row.key) seen.add(row.key)
      cursor = (result as { cursor?: string }).cursor ?? undefined
    } while (cursor && pages < 10)
    const ok = seen.size === FIXTURES
    console.log(`${ok ? "✓" : "✗"} pagination: ${pages} pages, ${seen.size} distinct keys`)
  } catch (error) {
    console.log(`✗ pagination: ${(error as Error).message}`)
  }

  // --- accumulated ghosts (known expired-serving bug) ---
  const head = BigInt((await rawRpc("eth_blockNumber", [])) as string)
  const all = await query(`kind = "query-probe"`)
  const expired = (all.data ?? []).filter((row) => BigInt(row.expiresAt ?? "0x0") <= head).length
  console.log(
    `\nghosts: ${all.data?.length ?? 0} query-probe entities on chain, ${expired} of them expired-but-served`,
  )
} finally {
  // Runs even if a probe throws, so an aborted run leaves no litter:
  // expired entities cannot be deleted afterwards (EntityExpired).
  await writer.mutateEntities({ deletes: fixtures.map((entityKey) => ({ entityKey })) })
  console.log(`fixtures deleted; query probe done`)
}
