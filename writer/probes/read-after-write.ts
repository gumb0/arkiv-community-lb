// Whether a query sees a write as soon as the write's receipt is out.
// Each round creates an entity, looks it up by key at once, then deletes
// it and looks again. A lookup that disagrees with the write is polled
// until it agrees, and the round records how long that took and which
// block each answer was read at. A lookup pinned to the receipt's block
// runs beside each miss, to tell a lagging view from a wrong one. Each
// round then creates a second entity and deletes it at once, with no
// lookup between, to see whether a write that touches an entity from
// the block before is accepted in that same moment.
//
// Env: WRITER_PRIVATE_KEY_FILE, ARKIV_RPC_URL, optional ARKIV_API_KEY,
// optional ROUNDS (default 40).
// Run: npm run read-after-write

import { bytes, defineSchema, ExpirationTime, str } from "@arkiv-network/sdk"
import type { Hex } from "viem"
import { createWriter } from "../src/writer.ts"
import { env, privateKeyFromFile } from "../src/env.ts"

const ROUNDS = Number(env("ROUNDS", false) || 40)
const TTL = ExpirationTime.fromBlocks(60)
const RUN = `r${Date.now().toString(36)}`
// How long a disagreeing lookup is polled before the round gives up.
const POLL_MS = 100
const GIVE_UP_MS = 15_000

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

/** Whether the key is found, and the block the node says it answered at. */
async function lookup(key: Hex, atBlock?: bigint): Promise<{ found: boolean; block: bigint }> {
  const options: Record<string, unknown> = { select: { key: true } }
  if (atBlock !== undefined) options.atBlock = `0x${atBlock.toString(16)}`
  const result = (await rawRpc("arkiv_query", [`$key = key(${key})`, options])) as {
    data?: unknown[]
    blockNumber: string
  }
  return { found: (result.data ?? []).length > 0, block: BigInt(result.blockNumber) }
}

async function receiptBlock(tx: Hex): Promise<bigint> {
  const receipt = (await rawRpc("eth_getTransactionReceipt", [tx])) as { blockNumber: string }
  return BigInt(receipt.blockNumber)
}

async function head(): Promise<bigint> {
  return BigInt((await rawRpc("eth_blockNumber", [])) as string)
}

type Miss = {
  round: number
  write: "create" | "delete"
  receipt: bigint
  head: bigint
  answeredAt: bigint
  pinned: string
  agreedAfterMs: number | undefined
  agreedAt: bigint | undefined
}

/**
 * The lookup right after a write, and if it disagrees, the polling until
 * it agrees. `want` is whether the key should be found.
 */
async function check(round: number, write: Miss["write"], key: Hex, tx: Hex, want: boolean): Promise<Miss | undefined> {
  const first = await lookup(key)
  if (first.found === want) return undefined
  const receipt = await receiptBlock(tx)
  const now = await head()
  // The same lookup at the receipt's block: right there means the view
  // behind the head lags, wrong there means the write is not in that block.
  let pinned: string
  try {
    pinned = (await lookup(key, receipt)).found === want ? "agrees" : "disagrees"
  } catch (error) {
    pinned = `refused (${error instanceof Error ? error.message : String(error)})`
  }
  const started = Date.now()
  for (;;) {
    const again = await lookup(key)
    if (again.found === want) {
      return { round, write, receipt, head: now, answeredAt: first.block, pinned, agreedAfterMs: Date.now() - started, agreedAt: again.block }
    }
    if (Date.now() - started > GIVE_UP_MS) {
      return { round, write, receipt, head: now, answeredAt: first.block, pinned, agreedAfterMs: undefined, agreedAt: undefined }
    }
    await new Promise((resolve) => setTimeout(resolve, POLL_MS))
  }
}

const Probe = defineSchema({
  kind: str().indexed(),
  run: str().indexed(),
  payload: bytes(),
})

const writer = await createWriter({ rpcUrl, apiKey: apiKey || undefined, privateKey: privateKeyFromFile() })
console.log(`address ${writer.address}, chain ${writer.chainId}, run ${RUN}, ${ROUNDS} rounds`)

const create = (round: number) =>
  writer.createEntity({
    schema: Probe,
    values: { kind: str("read-after-write"), run: str(RUN), payload: bytes(new TextEncoder().encode(`{"round":${round}}`)) },
    expiration: TTL,
  })

const misses: Miss[] = []
const refusedDeletes: string[] = []
for (let round = 1; round <= ROUNDS; round += 1) {
  const created = await create(round)
  const afterCreate = await check(round, "create", created.entityKey, created.txHash, true)
  const deleted = await writer.deleteEntity({ entityKey: created.entityKey })
  const afterDelete = await check(round, "delete", created.entityKey, deleted.txHash, false)
  for (const miss of [afterCreate, afterDelete]) {
    if (miss === undefined) continue
    misses.push(miss)
    console.log(
      `round ${miss.round} ${miss.write}: lookup answered at block ${miss.answeredAt}, ` +
        `receipt in ${miss.receipt}, head ${miss.head}; at the receipt's block it ${miss.pinned}; ` +
        (miss.agreedAfterMs === undefined
          ? `still wrong after ${GIVE_UP_MS} ms`
          : `right after ${miss.agreedAfterMs} ms, answered at block ${miss.agreedAt}`),
    )
  }

  // A refused delete leaves the entity to expire with its short life.
  const next = await create(round)
  try {
    await writer.deleteEntity({ entityKey: next.entityKey })
  } catch (error) {
    const said = error instanceof Error ? (error.message.split("\n")[0] ?? "") : String(error)
    refusedDeletes.push(said)
    console.log(`round ${round}: a delete right after the create was refused: ${said}`)
  }
}

const creates = misses.filter((miss) => miss.write === "create").length
const deletes = misses.length - creates
console.log(
  `\n${ROUNDS} rounds: ${creates} create${creates === 1 ? "" : "s"} not seen at once, ` +
    `${deletes} delete${deletes === 1 ? "" : "s"} still seen at once`,
)
const behind = misses.filter((miss) => miss.answeredAt < miss.receipt).length
console.log(`${behind} of ${misses.length} misses answered at a block before the write's receipt`)
console.log(`${refusedDeletes.length} of ${ROUNDS} deletes right after a create refused`)
