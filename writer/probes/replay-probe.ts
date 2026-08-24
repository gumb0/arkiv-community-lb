// Replay probe: is an already-mined transaction protected against replay?
// The create-dup probe showed the dev node executing one signed tx twice
// (consecutive-block double inclusion, account nonce not enforced). This
// asks the question directly, on any endpoint: mine a create, fetch its
// raw bytes, re-submit them, and see whether a second entity appears.
//
// The LB's relay decision leans on "same signed bytes => same hash,
// mempools dedupe, replay is safe" — this is that premise, tested.
//
// Env: WRITER_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY.
// Run: npm run replay
// Gas cost: one small create (plus one duplicate, if the bug is present).

import { ExpirationTime, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"

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

const writer = await createWriter({
  rpcUrl,
  apiKey: apiKey || undefined,
  privateKey: env("WRITER_PRIVATE_KEY") as Hex,
})
console.log(`replay probe: run ${RUN}, address ${writer.address}, chain ${writer.chainId}\n`)

const query = async () =>
  ((await rawRpc("arkiv_query", [
    `kind = str('replay-probe') AND run = str('${RUN}')`,
    { select: { key: true }, limit: "0x14" },
  ])) as { data?: { key: Hex }[] }).data ?? []

const created: Hex[] = []
try {
  const { entityKey, txHash } = await writer.createEntity({
    payload: new Uint8Array([1]),
    contentType: "application/octet-stream",
    attributes: { kind: str("replay-probe"), run: str(RUN) },
    expires: ExpirationTime.fromBlocks(600),
  })
  created.push(entityKey)
  console.log(`created ${entityKey}\n  tx ${txHash} (mined - receipt seen)`)

  const rawTx = (await rawRpc("eth_getRawTransactionByHash", [txHash])) as Hex
  console.log(`  raw tx: ${(rawTx.length - 2) / 2} bytes\n`)

  console.log(`re-submitting the same signed bytes...`)
  let outcome: string
  try {
    const resubmitted = (await rawRpc("eth_sendRawTransaction", [rawTx])) as Hex
    outcome = `ACCEPTED (returned ${resubmitted === txHash ? "the same hash" : `hash ${resubmitted}`})`
  } catch (error) {
    outcome = `rejected: ${(error as Error).message}`
  }
  console.log(`  ${outcome}\n`)

  // Give a re-execution time to mine, then count.
  await new Promise((resolve) => setTimeout(resolve, 8000))
  const rows = await query()
  console.log(`entities carrying this run's tag: ${rows.length} (created: 1)`)
  console.log(
    `\nverdict: ` +
      (rows.length > 1
        ? "REPLAYED AND RE-EXECUTED — a mined tx was executed again; replay protection absent"
        : "replay had no effect — the premise holds on this endpoint"),
  )
  for (const row of rows) if (!created.includes(row.key)) created.push(row.key)
} finally {
  if (created.length > 0) {
    await writer.executeBatch({ deletes: created.map((entityKey) => ({ entityKey })) })
    console.log(`cleanup: ${created.length} deleted; replay probe done`)
  }
}
