// receipts: writes the receipts for a transfer that landed without
// them. It checks and prints unless given the settle key.

import { formatEther, getAddress, type Hex } from "viem"
import { connectReader, connectWriter, type Reader } from "./chain.ts"
import { previousAddresses, settleAddress } from "./identity.ts"
import { RECEIPTS_PER_BATCH } from "./pay.ts"
import { connectPayout } from "./payout.ts"
import { check, parseReceiptsArgs, RECEIPTS_USAGE, type Found } from "./receipts.ts"
import { attrAddr, attrStr, byKind, creator, decodeCounter, decodeReceipt, KIND, receiptFor } from "./records.ts"

function required(name: string): string {
  const value = process.env[name]
  if (value === undefined || value === "") {
    throw new Error(`${name} is not set`)
  }
  return value
}

function optional(name: string): string | undefined {
  const value = process.env[name]
  return value === undefined || value === "" ? undefined : value
}

/** The LB's closed counter record under one key, and whether a receipt names it. */
async function find(reader: Reader, lb: Hex, payers: Hex[], key: Hex): Promise<Found> {
  const rows = await reader.query(
    byKind(KIND.counter, creator(lb), attrStr("state", "closed"), `$key = key(${key})`),
  )
  const row = rows[0]
  return {
    recordKey: key,
    record: row === undefined ? undefined : decodeCounter(row),
    receipted: await receipted(reader, payers, key),
  }
}

/** Whether a receipt from any of these addresses names the record. */
async function receipted(reader: Reader, payers: Hex[], key: Hex): Promise<boolean> {
  for (const payer of payers) {
    const rows = await reader.query(byKind(KIND.receipt, creator(payer), `counter = key(${key})`))
    if (rows.length > 0) return true
  }
  return false
}

/** What the provider's receipts that name this transfer already account for, in wei. */
async function receiptedFor(
  reader: Reader,
  payers: Hex[],
  provider: Hex,
  payout: { chainId: number; tx: Hex },
): Promise<bigint> {
  let total = 0n
  for (const payer of payers) {
    const rows = await reader.query(byKind(KIND.receipt, creator(payer), attrAddr("provider", provider)))
    for (const receipt of rows.map(decodeReceipt)) {
      if (receipt.payout.chainId === payout.chainId && receipt.payout.tx.toLowerCase() === payout.tx.toLowerCase()) {
        total += receipt.amountWei
      }
    }
  }
  return total
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2)
  if (argv.includes("--help") || argv.includes("-h")) {
    console.log(RECEIPTS_USAGE)
    return
  }
  const run = parseReceiptsArgs(argv)
  const key = optional("SETTLE_PRIVATE_KEY") as Hex | undefined

  // The same configuration as settle, and the same settle address.
  const arkivUrl = required("ARKIV_RPC_URL")
  const arkivKey = optional("ARKIV_API_KEY")
  const reader = await connectReader(arkivUrl, arkivKey)
  const lb = getAddress(required("LB_ADDRESS")) as Hex
  const payout = await connectPayout({
    rpcUrl: required("PAYOUT_RPC_URL"),
    chainId: Number(required("PAYOUT_CHAIN_ID")),
    token: getAddress(required("GLM_TOKEN_ADDRESS")) as Hex,
    privateKey: key,
  })
  const settle = settleAddress(payout.address, optional("SETTLE_ADDRESS"))
  const payers = [settle, ...previousAddresses(settle, optional("SETTLE_PREVIOUS_ADDRESSES"))]

  // Everything checked before anything is signed.
  const found: Found[] = []
  for (const record of run.records) found.push(await find(reader, lb, payers, record))
  const provider = found.find((entry) => entry.record !== undefined)?.record?.provider
  const receiptedWei =
    provider === undefined
      ? 0n
      : await receiptedFor(reader, payers, provider, { chainId: payout.chain.id, tx: run.tx })
  const checked = check({ tx: run.tx, moved: await payout.moved(run.tx), settle, found, receiptedWei })
  if (!checked.ok) {
    for (const problem of checked.problems) console.log(`Refusing: ${problem}`)
    process.exitCode = 1
    return
  }
  console.log(
    `${run.tx} paid ${formatEther(checked.amountWei)} GLM to ${checked.provider} for ${checked.records.length} record${checked.records.length === 1 ? "" : "s"}:`,
  )
  for (const record of checked.records) console.log(`    ${record.key}  ${record.count} requests`)

  if (key === undefined) {
    console.log("Rehearsal: nothing was written. Give SETTLE_PRIVATE_KEY to write the receipts.")
    return
  }

  // Written as a run writes them: every batch is tried, whatever the
  // one before it did, and what landed is told by reading back.
  const writer = await connectWriter(arkivUrl, arkivKey, key)
  const receipts = checked.records.map((record) => receiptFor(record, { chainId: payout.chain.id, tx: run.tx }))
  let refused: unknown
  for (let at = 0; at < receipts.length; at += RECEIPTS_PER_BATCH) {
    try {
      await writer.write(receipts.slice(at, at + RECEIPTS_PER_BATCH))
    } catch (error) {
      refused = error
    }
  }
  const missing: Hex[] = []
  for (const record of checked.records) {
    if (!(await receipted(reader, [settle], record.key))) missing.push(record.key)
  }
  if (missing.length > 0) {
    const why = refused === undefined ? "" : `: ${refused instanceof Error ? refused.message : String(refused)}`
    console.log(`No receipt for ${missing.length} of ${receipts.length} records${why}`)
    for (const recordKey of missing) console.log(`    ${recordKey}`)
    console.log("Run this command again with the records above. Do not run settle before.")
    process.exitCode = 1
    return
  }
  console.log(`Wrote ${receipts.length} receipt${receipts.length === 1 ? "" : "s"}; the next settle run will not pay these records.`)
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error))
  process.exit(1)
})
