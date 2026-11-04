// Pay probe: settle's real ledger and pay against a node, with only the
// GLM transfer faked. It writes closed counter records the way the LB
// does, for one provider made up for the run, enough of them that every
// read crosses a page; then it checks that they are owed, that one run
// pays them with one transfer and a receipt each, and that afterwards
// nothing is owed and every receipt reads back once.
//
// One key plays both the LB and settle. Only this run's provider is paid,
// so records already on the chain do not matter. The counter records are
// deleted at the end; the receipts are readonly and stay for the chain's
// longest lifetime, so run it on a dev node rather than a network.
//
// Env: SETTLE_PRIVATE_KEY, ARKIV_RPC_URL, optional ARKIV_API_KEY (sent as an
// Authorization: Bearer header).
// Run: npm run pay-probe

import {
  addr,
  bytes,
  createWalletClient,
  defineSchema,
  ExpirationTime,
  i32,
  key,
  str,
} from "@arkiv-network/sdk"
import { defineChain, http, stringToHex, toHex, type Hex } from "viem"
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts"
import { connectReader, connectWriter } from "../src/chain.ts"
import { ledger, type Ledger, type Owed } from "../src/ledger.ts"
import { pay } from "../src/pay.ts"
import { attrAddr, byKind, creator, decodeReceipt, KIND } from "../src/records.ts"

const RECORDS = 120 // more than a page, for the ledger and for the receipts
const BATCH = 50 // creates per transaction, under the node's size cap
const LIFE = ExpirationTime.fromBlocks(2000)
const WEI_PER_CALL = 1_000_000_000_000_000n

function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name}`)
    process.exit(1)
  }
  return value
}
const rpcUrl = env("ARKIV_RPC_URL").replace(/\/+$/, "")
const apiKey = env("ARKIV_API_KEY", false) || undefined
const privateKey = env("SETTLE_PRIVATE_KEY") as Hex

// The counter record as the LB writes it (docs/ENTITIES.md).
const Counter = defineSchema({
  kind: str().indexed(),
  v: i32().indexed(),
  agreement: key().indexed(),
  provider: addr().indexed(),
  state: str().indexed(),
  payload: bytes(),
})

let failures = 0
function check(cond: boolean, what: string) {
  console.log(`${cond ? "✓" : "✗"} ${what}`)
  if (!cond) failures += 1
}

// A query answer labeled with a write's block can miss that write for a
// moment after its receipt, so a read that follows a write repeats until
// it shows the write, for up to 2 s; the checks judge the last answer.
async function settled<T>(read: () => Promise<T>, done: (value: T) => boolean): Promise<T> {
  const until = Date.now() + 2000
  for (;;) {
    const value = await read()
    if (done(value) || Date.now() > until) return value
    await new Promise((resolve) => setTimeout(resolve, 100))
  }
}

/** What the ledger says the run's provider is owed, if anything. */
function owedTo(plan: Ledger): Owed | undefined {
  return plan.owed.find((o) => o.provider.toLowerCase() === provider.toLowerCase())
}

const reader = await connectReader(rpcUrl, apiKey)
const writer = await connectWriter(rpcUrl, apiKey, privateKey)
const me = writer.address
const provider = privateKeyToAccount(generatePrivateKey()).address
console.log(`pay probe: address ${me}, chain ${reader.chainId}, provider ${provider}\n`)

const lb = createWalletClient({
  chain: defineChain({
    id: reader.chainId,
    name: `arkiv-${reader.chainId}`,
    nativeCurrency: { name: "Golem", symbol: "GLM", decimals: 18 },
    rpcUrls: { default: { http: [rpcUrl] } },
  }),
  transport: http(rpcUrl, { fetchOptions: apiKey ? { headers: { authorization: `Bearer ${apiKey}` } } : undefined }),
  account: privateKeyToAccount(privateKey),
  pollingInterval: 250,
})

const counters: Hex[] = []
try {
  for (let at = 0; at < RECORDS; at += BATCH) {
    const creates = Array.from({ length: Math.min(BATCH, RECORDS - at) }, (_, j) => ({
      schema: Counter,
      values: {
        kind: str(KIND.counter),
        v: i32(1),
        agreement: key(toHex(crypto.getRandomValues(new Uint8Array(32)))),
        provider: addr(provider),
        state: str("closed"),
        payload: bytes(
          stringToHex(
            JSON.stringify({
              count: at + j + 1,
              wei_per_call: WEI_PER_CALL.toString(),
              opened_block: 1,
              closed_block: 2,
            }),
          ),
        ),
      },
      expiration: LIFE,
    }))
    counters.push(...(await lb.executeBatch({ creates })).createdEntities)
  }

  const before = await settled(
    () => ledger(reader, me, me),
    (plan) => owedTo(plan)?.records.length === RECORDS,
  )
  const owed = owedTo(before)
  check(owed?.records.length === RECORDS, `${RECORDS} closed records owed to the run's provider`)
  if (owed === undefined) throw new Error("nothing owed to the run's provider")

  let transfers = 0
  const fakeTx = `0x${"ab".repeat(32)}` as Hex
  const paid = await pay({
    plan: { owed: [owed], paid: 0, totalOwedWei: owed.amountWei },
    chainId: 1, // the fake transfer's chain, as the receipts name it
    transfer: async () => {
      transfers += 1
      return fakeTx
    },
    write: writer.write,
    log: (line) => console.log(`  ${line}`),
    blockedBy: [],
  })
  check(transfers === 1 && paid.length === 1, "one transfer for the provider")
  check(paid[0]?.receipted === true && paid[0].records === RECORDS, "a receipt for every record")

  const after = await settled(
    () => ledger(reader, me, me),
    (plan) => owedTo(plan) === undefined,
  )
  check(owedTo(after) === undefined, "nothing owed to the provider afterwards")
  const receipts = await settled(
    async () =>
      (await reader.query(byKind(KIND.receipt, creator(me), attrAddr("provider", provider)))).map(
        decodeReceipt,
      ),
    (read) => read.length === RECORDS,
  )
  const named = new Set(receipts.map((r) => r.counter.toLowerCase()))
  check(
    receipts.length === RECORDS &&
      named.size === RECORDS &&
      counters.every((c) => named.has(c.toLowerCase())),
    `every receipt reads back once, each naming one of the run's records`,
  )
  check(
    receipts.every((r) => r.payout.tx === fakeTx && r.amountWei > 0n),
    "the receipts carry the transfer and the amount",
  )
} finally {
  for (let at = 0; at < counters.length; at += BATCH) {
    await lb.executeBatch({ deletes: counters.slice(at, at + BATCH).map((entityKey) => ({ entityKey })) })
  }
  console.log(`\ncleanup: ${counters.length} counter records deleted; the receipts stay`)
}

if (failures > 0) {
  console.error(`\npay probe: ${failures} check(s) failed`)
  process.exit(1)
}
console.log("\npay probe: all checks passed")
