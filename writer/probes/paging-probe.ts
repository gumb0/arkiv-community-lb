// Paging probe: a walk over a whole query, a page of keys after the last
// one seen, `AND $key > key(<last>) ORDER BY $key`. Settle reads its
// records and receipts this way, so this checks what that walk relies on:
// keys come in order, the bound is strict, a walk returns every row once,
// and rows expiring between two pages cost it no live row.
//
// Every check runs with two readers that must agree: the SDK's query, the
// call settle makes, and a raw `arkiv_query`, the way the LB's Rust side
// reads. The other probes read raw only, since their reader is the LB.
//
// For contrast it prints how many live rows a walk by offset misses over
// the same expiry. That is not asserted: it shows why the walk is by key.
//
// Lifetimes are in blocks, so the probe runs on a fast dev chain and on a
// 2 s network alike; on the latter it waits about a minute for the
// short-lived rows to expire. It deletes what is left at the end.
//
// Env: WRITER_PRIVATE_KEY_FILE, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header).
// Run: npm run paging

import { bytes, createPublicClient, defineSchema, ExpirationTime, i32, str } from "@arkiv-network/sdk"
import { http, type Hex } from "viem"
import { createWriter } from "../src/writer.ts"
import { env, privateKeyFromFile } from "../src/env.ts"

const RUN = `p${Date.now().toString(36)}`
const PAGE = 100
const ROWS = 250 // three pages, the last one short, before and after the expiry
const SHORT_LIFE = 30 // blocks; every fifth row of the first transaction
const LONG_LIFE = 600 // blocks; outlives the probe on a fast dev chain too

const rpcUrl = env("ARKIV_RPC_URL").replace(/\/+$/, "")
const apiKey = env("ARKIV_API_KEY", false)
const headers = apiKey ? { authorization: `Bearer ${apiKey}` } : undefined

const Fixture = defineSchema({
  kind: str().indexed(),
  run: str().indexed(),
  rank: i32().indexed(),
  payload: bytes(),
})

let rpcId = 0
async function rawRpc(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++rpcId, method, params }),
  })
  const body = (await res.json()) as { result?: unknown; error?: { message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message}`)
  return body.result
}

/** One page of keys for a query: the same question asked two ways. */
type Reader = { name: string; page(text: string, offset?: number): Promise<Hex[]> }

const client = createPublicClient({ transport: http(rpcUrl, { fetchOptions: { headers } }) })
const viaSdk: Reader = {
  name: "sdk",
  page: async (text, offset) =>
    (await client.query(text, { limit: PAGE, offset, select: { $key: true } })).entities.map(
      (row) => row.$key,
    ),
}
const viaRaw: Reader = {
  name: "raw",
  page: async (text, offset) => {
    const options = { select: { key: true }, limit: `0x${PAGE.toString(16)}` }
    const result = (await rawRpc("arkiv_query", [
      text,
      offset === undefined ? options : { ...options, offset: `0x${offset.toString(16)}` },
    ])) as { data?: { key: Hex }[] }
    return (result.data ?? []).map((row) => row.key)
  },
}
const readers = [viaSdk, viaRaw]

let failures = 0
function check(cond: boolean, what: string) {
  console.log(`${cond ? "✓" : "✗"} ${what}`)
  if (!cond) failures += 1
}

const ascending = (keys: Hex[]) => keys.every((k, i) => i === 0 || BigInt(keys[i - 1] as Hex) < BigInt(k))
const after = (q: string, last?: Hex) => `${q}${last ? ` AND $key > key(${last})` : ""} ORDER BY $key`

/** A walk by key, a page at a time, so two can run side by side. */
type Walk = { reader: Reader; keys: Hex[]; done: boolean }
async function step(walk: Walk, q: string) {
  const page = await walk.reader.page(after(q, walk.keys.at(-1)))
  walk.keys.push(...page)
  if (page.length < PAGE) walk.done = true
}
async function walkAll(reader: Reader, q: string): Promise<Hex[]> {
  const walk: Walk = { reader, keys: [], done: false }
  while (!walk.done) await step(walk, q)
  return walk.keys
}

const count = async (q: string) =>
  Number(((await rawRpc("arkiv_query", [q, { select: { count: true } }])) as { count: Hex }).count)

const writer = await createWriter({ rpcUrl, apiKey: apiKey || undefined, privateKey: privateKeyFromFile() })
console.log(`paging probe: address ${writer.address}, chain ${writer.chainId}, run ${RUN}\n`)
const mine = `kind = str('paging-probe') AND run = str('${RUN}')`

try {
  // The short-lived rows go in the first transaction, so they come first
  // in creation order: that is what makes an offset walk skip.
  const document = (i: number) => bytes(new TextEncoder().encode(JSON.stringify({ i })))
  let short = 0
  for (let at = 0; at < ROWS; at += PAGE) {
    const creates = Array.from({ length: Math.min(PAGE, ROWS - at) }, (_, j) => {
      const i = at + j
      const life = at === 0 && j % 5 === 0 ? SHORT_LIFE : LONG_LIFE
      if (life === SHORT_LIFE) short += 1
      return {
        schema: Fixture,
        values: { kind: str("paging-probe"), run: str(RUN), rank: i32(i), payload: document(i) },
        expiration: ExpirationTime.fromBlocks(life),
      }
    })
    await writer.executeBatch({ creates })
  }
  check((await count(mine)) === ROWS, `${ROWS} rows created, ${short} of them short-lived`)

  // First, while the short-lived rows still live: both walks and an
  // offset walk read their first page, the short-lived rows expire, then
  // all three finish.
  console.log("\nrows expiring between pages:")
  const walks: Walk[] = readers.map((reader) => ({ reader, keys: [], done: false }))
  for (const walk of walks) await step(walk, mine)
  const byOffset = await viaRaw.page(mine, 0)
  // Otherwise nothing expired between the pages, and the checks below
  // would pass with nothing tested.
  check((await count(mine)) === ROWS, "every row was alive when the first pages were read")
  const deadline = Date.now() + 10 * 60_000
  while ((await count(mine)) > ROWS - short) {
    if (Date.now() > deadline) throw new Error("the short-lived rows did not expire within ten minutes")
    await new Promise((resolve) => setTimeout(resolve, 1000))
  }
  for (const walk of walks) while (!walk.done) await step(walk, mine)
  for (let offset = PAGE; ; offset += PAGE) {
    const page = await viaRaw.page(mine, offset)
    byOffset.push(...page)
    if (page.length < PAGE) break
  }
  const live = await walkAll(viaRaw, mine)
  for (const walk of walks) {
    const missed = live.filter((k) => !walk.keys.includes(k)).length
    check(missed === 0, `${walk.reader.name}: the walk by key missed no live row (${short} expired)`)
  }
  const missedByOffset = live.filter((k) => !byOffset.includes(k)).length
  console.log(`  for contrast, a walk by offset missed ${missedByOffset} live rows`)

  // Then the walk itself, over the rows that are left.
  const rows = ROWS - short
  for (const reader of readers) {
    console.log(`\n${reader.name}:`)
    const first = await reader.page(after(mine))
    check(first.length === PAGE && ascending(first), "ORDER BY $key returns a full page of ascending keys")
    const bound = first[49] as Hex
    const next = await reader.page(after(mine, bound))
    check(next[0] === first[50] && !next.includes(bound), "$key > key(…) is a strict bound")
    const all = await walkAll(reader, mine)
    check(
      all.length === rows && new Set(all).size === rows && ascending(all),
      "a walk returns every row once, in order",
    )
    const creator = `${mine} AND $creator = addr(${writer.address.toLowerCase()})`
    check((await reader.page(after(creator))).join() === first.join(), "the walk combines with $creator")
  }
  console.log("")
  check(
    (await walkAll(viaSdk, mine)).join() === (await walkAll(viaRaw, mine)).join(),
    "the two readers walk the same keys",
  )
} finally {
  const left = await walkAll(viaRaw, mine)
  for (let at = 0; at < left.length; at += PAGE) {
    await writer.executeBatch({ deletes: left.slice(at, at + PAGE).map((entityKey) => ({ entityKey })) })
  }
  console.log(`\ncleanup: ${left.length} rows deleted`)
}

if (failures > 0) {
  console.error(`\npaging probe: ${failures} check(s) failed`)
  process.exit(1)
}
console.log("\npaging probe: all checks passed")
