// Concurrency probe: what happens when one key sends transactions in
// parallel. Phase 1 is the sequential baseline; phase 2 races two creates
// with the default account (expect a tx-nonce collision — observe its exact
// shape); phase 3 races again with viem's nonceManager on the account.
// Observational: outcomes are printed in full, only cleanup is asserted.
//
// Env: PRIVATE_KEY, RPC_URL, optional API_KEY (sent as Authorization: Bearer).
// Run: npm run concurrency

import { createWalletClient, ExpirationTime, jsonToPayload, str } from "@arkiv-network/sdk"
import { createPublicClient, defineChain, http, type Chain, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"
import { nonceManager } from "viem/nonce"

const TTL = ExpirationTime.fromMinutes(5)
const RUN = `r${Date.now().toString(36)}`

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
const privateKey = env("PRIVATE_KEY") as Hex

async function rawRpc(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  })
  const body = (await res.json()) as { result?: unknown; error?: { message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message}`)
  return body.result
}

const transport = http(rpcUrl, {
  fetchOptions: apiKey ? { headers: { authorization: `Bearer ${apiKey}` } } : undefined,
})
const chainId = await createPublicClient({ transport }).getChainId()
const chain: Chain = defineChain({
  id: chainId,
  name: `arkiv-${chainId}`,
  nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
  rpcUrls: { default: { http: [rpcUrl] } },
})

function makeWallet(withNonceManager: boolean) {
  const account = withNonceManager
    ? privateKeyToAccount(privateKey, { nonceManager })
    : privateKeyToAccount(privateKey)
  return createWalletClient({ chain, transport, account })
}

const create = (wallet: ReturnType<typeof makeWallet>, tag: string) =>
  wallet.createEntity({
    payload: jsonToPayload({ probe: "concurrency", tag, run: RUN }),
    contentType: "application/json",
    attributes: { kind: str("concurrency-probe"), run: str(RUN), tag: str(tag) },
    expires: TTL,
  })

function describe(outcome: PromiseSettledResult<{ entityKey: Hex }>): string {
  if (outcome.status === "fulfilled") return `OK ${outcome.value.entityKey.slice(0, 18)}…`
  const error = outcome.reason as Error
  const cause = error.cause as Error | undefined
  const causeCause = cause?.cause as Error | undefined
  return (
    `${error.constructor.name}: ${error.message.slice(0, 100)}` +
    (cause ? `\n      cause: ${cause.constructor.name}: ${(cause.message ?? "").slice(0, 100)}` : "") +
    (causeCause ? `\n      cause.cause: ${causeCause.constructor.name}` : "")
  )
}

async function onChainCount(): Promise<number> {
  const result = (await rawRpc("arkiv_query", [
    `kind = str('concurrency-probe') AND run = str('${RUN}')`,
    { select: { key: true } },
  ])) as { data?: { key: Hex }[] }
  return result.data?.length ?? 0
}

console.log(`concurrency probe: run ${RUN}, chain ${chainId}\n`)
const plain = makeWallet(false)

console.log("phase 1: two sequential creates (baseline)")
await create(plain, "seq-1")
await create(plain, "seq-2")
console.log(`  both landed; on-chain count: ${await onChainCount()}\n`)

console.log("phase 2: Promise.all x2, default account (expect a nonce race)")
const raced = await Promise.allSettled([create(plain, "race-1"), create(plain, "race-2")])
raced.forEach((outcome, i) => console.log(`  race-${i + 1}: ${describe(outcome)}`))
console.log(`  on-chain count now: ${await onChainCount()}\n`)

console.log("phase 3: Promise.all x2, account with viem nonceManager")
const managed = makeWallet(true)
const managedRace = await Promise.allSettled([create(managed, "nm-1"), create(managed, "nm-2")])
managedRace.forEach((outcome, i) => console.log(`  nm-${i + 1}: ${describe(outcome)}`))
const finalCount = await onChainCount()
console.log(`  on-chain count now: ${finalCount}\n`)

// Cleanup: delete whatever actually landed, whoever created it.
const rows = (await rawRpc("arkiv_query", [
  `kind = str('concurrency-probe') AND run = str('${RUN}')`,
  { select: { key: true } },
])) as { data?: { key: Hex }[] }
const keys = (rows.data ?? []).map((row) => row.key)
if (keys.length > 0) {
  await plain.mutateEntities({ deletes: keys.map((entityKey) => ({ entityKey })) })
}
console.log(`cleanup: ${keys.length} entities deleted; concurrency probe done`)
