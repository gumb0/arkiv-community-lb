// Whether a write sent right after another write's receipt costs more
// than the gas estimate it was sent with. Each round, three creates with
// the same attributes:
//   settled  — a second after the round before; the estimate is the limit.
//   generous — right after a delete's receipt, with a limit far above any
//              estimate, so it cannot run out: its gasUsed is what such a
//              create needs.
//   estimated — right after a delete's receipt again, the estimate as the
//              limit, the way every SDK write is sent; a revert here is
//              the failure being measured.
// Each create is deleted at once. Receipts are polled every 250 ms, so a
// write follows the one before it soon after the block, where a lagging
// view would show.
//
// Env: WRITER_PRIVATE_KEY_FILE, ARKIV_RPC_URL, optional ARKIV_API_KEY,
// optional ROUNDS (default 20).
// Run: npm run gas-after-write

import { bytes, createWalletClient, defineSchema, ExpirationTime, str } from "@arkiv-network/sdk"
import { defineChain, http, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"
import { env, privateKeyFromFile } from "../src/env.ts"

const ROUNDS = Number(env("ROUNDS", false) || 20)
const TTL = ExpirationTime.fromBlocks(60)
const RUN = `g${Date.now().toString(36)}`
const GENEROUS = 1_000_000n

const rpcUrl = env("ARKIV_RPC_URL").replace(/\/+$/, "")
const apiKey = env("ARKIV_API_KEY", false)
const headers = apiKey ? { authorization: `Bearer ${apiKey}` } : undefined

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

const chainId = Number(BigInt((await rawRpc("eth_chainId", [])) as string))
const wallet = createWalletClient({
  chain: defineChain({
    id: chainId,
    name: `arkiv-${chainId}`,
    nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
    rpcUrls: { default: { http: [rpcUrl] } },
  }),
  transport: http(rpcUrl, { fetchOptions: headers ? { headers } : undefined }),
  account: privateKeyToAccount(privateKeyFromFile()),
  pollingInterval: 250,
})

const Probe = defineSchema({
  kind: str().indexed(),
  run: str().indexed(),
  payload: bytes(),
})

/** One write as it went: its limit, what it used, and whether it reverted. */
type Sent = { limit: bigint; used: bigint; reverted: boolean }

/** The gas limit and use of a mined transaction. */
async function gasOf(tx: Hex): Promise<Sent> {
  const sent = (await rawRpc("eth_getTransactionByHash", [tx])) as { gas: string }
  const receipt = (await rawRpc("eth_getTransactionReceipt", [tx])) as { gasUsed: string; status: string }
  return { limit: BigInt(sent.gas), used: BigInt(receipt.gasUsed), reverted: receipt.status !== "0x1" }
}

/**
 * Runs a write and returns how it went, with the entity's key when a
 * create landed. A revert is a result, not an error: the SDK throws it
 * with the transaction's hash.
 */
async function send(write: () => Promise<{ txHash: Hex; entityKey?: Hex }>): Promise<Sent & { key?: Hex }> {
  try {
    const result = await write()
    return { ...(await gasOf(result.txHash)), key: result.entityKey }
  } catch (error) {
    const tx = (error as { txHash?: Hex }).txHash
    if (tx === undefined) throw error
    return gasOf(tx)
  }
}

const create = (round: number, gas?: bigint) => () =>
  wallet.createEntity(
    Probe,
    { kind: str("gas-after-write"), run: str(RUN), payload: bytes(new TextEncoder().encode(`{"round":${round}}`)) },
    { expiration: TTL },
    gas === undefined ? undefined : { gas },
  )
const remove = (key: Hex) => () => wallet.deleteEntity({ entityKey: key })

const runs: Record<"settled" | "generous" | "estimated" | "delete", Sent[]> = {
  settled: [],
  generous: [],
  estimated: [],
  delete: [],
}

console.log(`address ${wallet.account.address}, chain ${chainId}, run ${RUN}, ${ROUNDS} rounds`)
for (let round = 1; round <= ROUNDS; round += 1) {
  await new Promise((resolve) => setTimeout(resolve, 1000))
  for (const kind of ["settled", "generous", "estimated"] as const) {
    const made = await send(create(round, kind === "generous" ? GENEROUS : undefined))
    runs[kind].push(made)
    if (made.reverted) {
      console.log(`round ${round} ${kind} create reverted: limit ${made.limit}, used ${made.used}`)
      continue
    }
    const gone = await send(remove(made.key as Hex))
    runs.delete.push(gone)
    if (gone.reverted) console.log(`round ${round} delete after the ${kind} create reverted: limit ${gone.limit}`)
  }
}

const range = (values: bigint[]) =>
  values.length === 0 ? "-" : `${values.reduce((a, b) => (a < b ? a : b))}..${values.reduce((a, b) => (a > b ? a : b))}`
console.log("")
for (const [kind, sent] of Object.entries(runs)) {
  const landed = sent.filter((s) => !s.reverted)
  console.log(
    `${kind.padEnd(9)} ${sent.length} sent, ${sent.length - landed.length} reverted; ` +
      `limit ${range(sent.map((s) => s.limit))}, used ${range(landed.map((s) => s.used))}`,
  )
}
const ceiling = runs.settled.filter((s) => !s.reverted).map((s) => s.limit)
const highest = ceiling.length === 0 ? 0n : ceiling.reduce((a, b) => (a > b ? a : b))
const over = runs.generous.filter((s) => !s.reverted && s.used > highest).length
console.log(`${over} of ${runs.generous.length} creates right after a delete used more than any settled create's estimate (${highest})`)
