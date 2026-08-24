// Page-budget probe: how big can one arkiv_query page get? The LB's
// max_response_size must exceed the node's page size, or legitimate query
// pages get truncated — and whether arkiv-reth bounds pages by bytes at
// all (or only by the 200-row limit) is unverified. This measures it:
// create fat-payload fixtures, ask for them all in one page, report rows
// and bytes.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as
// an Authorization: Bearer header);
// PAGE_FIXTURES (default 60), PAGE_PAYLOAD_KB (default 100).
// Run: npm run page-budget
//
// With the defaults the fixtures hold ~6 MB of raw payload (~12 MB as hex
// in the response), enough to detect a byte budget up to ~12 MB. A clean
// "no budget hit" result is a lower bound, not proof of none — raise
// PAGE_FIXTURES to push the bound.

import { ExpirationTime, i32, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

const TTL = ExpirationTime.fromBlocks(600) // blocks, so a faster chain means the same thing
const RUN = `r${Date.now().toString(36)}`
const FIXTURES = Number(process.env.PAGE_FIXTURES ?? "60")
const PAYLOAD_KB = Number(process.env.PAGE_PAYLOAD_KB ?? "100")

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

/** One raw JSON-RPC call, returning the parsed result AND the response size. */
async function rawRpcMeasured(
  method: string,
  params: unknown[],
): Promise<{ result: unknown; bytes: number }> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  })
  const text = await res.text()
  const body = JSON.parse(text) as { result?: unknown; error?: { message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message}`)
  return { result: body.result, bytes: Buffer.byteLength(text, "utf8") }
}

function assert(cond: boolean, what: string): asserts cond {
  if (!cond) throw new Error(`assertion failed: ${what}`)
}

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("WRITER_PRIVATE_KEY") as Hex,
})
console.log(
  `page-budget probe: ${FIXTURES} fixtures x ${PAYLOAD_KB} KB, run ${RUN}, ` +
    `address ${writer.address}, chain ${writer.chainId}\n`,
)

// One create per transaction: a fat payload nearly fills the node's
// 128 KiB raw-tx cap on its own, so batching would burst it.
const payload = new Uint8Array(PAYLOAD_KB * 1024).fill(0x61) // 'a'
const created: Hex[] = []
const startedAll = Date.now()
try {
  for (let i = 0; i < FIXTURES; i++) {
    const { entityKey } = await writer.createEntity({
      payload,
      contentType: "application/octet-stream",
      attributes: { kind: str("page-budget"), run: str(RUN), rank: i32(i) },
      expires: TTL,
    })
    created.push(entityKey)
    if ((i + 1) % 10 === 0) console.log(`  ${i + 1}/${FIXTURES} created`)
  }
  console.log(`fixtures on chain in ${((Date.now() - startedAll) / 1000).toFixed(0)}s\n`)

  const { result, bytes } = await rawRpcMeasured("arkiv_query", [
    `kind = str('page-budget') AND run = str('${RUN}')`,
    {
      select: { key: true, payload: true, attributes: { run: true, kind: true, rank: true } },
      limit: "0xc8", // 200, the row cap
    },
  ])
  type Row = { key: Hex; attributes?: { name: string; value: unknown }[] }
  const page = result as { data?: Row[]; cursor?: string }
  const rows = page.data?.length ?? 0
  const mb = (bytes / 1024 / 1024).toFixed(2)

  // Distinctness and scoping: a run-scoped query must return exactly our
  // fixtures, once each. 2026-08-24: runs returned 109 and 112 rows from
  // 60 fixtures — characterize instead of inferring page size. Keys are
  // compared case-normalized to rule out a casing mirage.
  const ours = new Set(created.map((key) => key.toLowerCase()))
  const counts = new Map<string, number>()
  for (const row of page.data ?? []) {
    const key = row.key.toLowerCase()
    counts.set(key, (counts.get(key) ?? 0) + 1)
  }
  const duplicated = [...counts.entries()].filter(([, n]) => n > 1)
  const foreign = (page.data ?? []).filter((row) => !ours.has(row.key.toLowerCase()))

  console.log(`one page, limit 200, payload selected:`)
  console.log(`  rows returned:  ${rows} of ${FIXTURES} fixtures`)
  console.log(
    `  distinct keys (case-normalized): ${counts.size}; duplicated: ${duplicated.length}; not ours: ${foreign.length}`,
  )
  if (duplicated.length > 0) {
    const [key, n] = duplicated[0]!
    console.log(`  e.g. ${key} appears ${n}x`)
  }
  // What do the foreign rows claim to be? If their run/kind match ours,
  // the node fabricated keys; if they differ, the predicate leaked.
  for (const row of foreign.slice(0, 3)) {
    const attr = (name: string) => row.attributes?.find((a) => a.name === name)?.value
    console.log(
      `  foreign: ${row.key}  kind=${JSON.stringify(attr("kind"))} ` +
        `run=${JSON.stringify(attr("run"))} rank=${JSON.stringify(attr("rank"))}`,
    )
  }
  console.log(`  response bytes: ${bytes} (${mb} MB, ~${Math.round(bytes / Math.max(rows, 1) / 1024)} KB/row)`)
  console.log(`  cursor present: ${page.cursor !== undefined}`)

  if (rows < FIXTURES) {
    console.log(
      `\n=> the node bounded the page: ~${mb} MB / ${rows} rows is the ` +
        `page budget at this payload size`,
    )
    assert(page.cursor !== undefined, "a bounded page carries a cursor for the rest")
  } else {
    console.log(
      `\n=> no byte budget hit at ${mb} MB — a lower bound only; ` +
        `raise PAGE_FIXTURES to push it`,
    )
  }
} finally {
  if (created.length > 0) {
    await writer.executeBatch({ deletes: created.map((entityKey) => ({ entityKey })) })
    // Verify the world after cleanup: any page-budget row still visible,
    // from any run, is a leftover the run-scoped queries above could not
    // explain.
    const { result } = await rawRpcMeasured("arkiv_query", [
      `kind = str('page-budget')`,
      { select: { key: true }, limit: "0xc8" },
    ])
    const remaining = (result as { data?: unknown[] }).data?.length ?? 0
    console.log(
      `\ncleanup: ${created.length} fixtures deleted; ` +
        `page-budget rows still visible (any run): ${remaining}; probe done`,
    )
  }
}
