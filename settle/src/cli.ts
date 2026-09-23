// settle: pays each closed counter record the LB wrote. It rehearses
// unless told to pay, and a rehearsal needs no key at all.

import { formatEther, getAddress, type Hex } from "viem"
import { parseArgs, USAGE } from "./args.ts"
import { connectReader, connectWriter } from "./chain.ts"
import { settleAddress } from "./identity.ts"
import { ledger } from "./ledger.ts"
import { pay } from "./pay.ts"
import { connectPayout, problems } from "./payout.ts"

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

async function main(): Promise<void> {
  const run = parseArgs(process.argv.slice(2))
  if (run.help) {
    console.log(USAGE)
    return
  }
  const key = optional("SETTLE_PRIVATE_KEY") as Hex | undefined
  if (run.paying && key === undefined) {
    throw new Error("--pay needs SETTLE_PRIVATE_KEY")
  }

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

  const plan = await ledger(reader, lb, settle)
  console.log(`Chain ${reader.chainId}, the LB's records under ${lb}`)
  for (const entry of plan.owed) {
    console.log(
      `${entry.provider}  ${formatEther(entry.amountWei)} GLM  ${entry.records.length} record${entry.records.length === 1 ? "" : "s"}`,
    )
    for (const record of entry.records) {
      console.log(
        `    ${record.key}  ${record.count} requests  blocks ${record.openedBlock}-${record.closedBlock}`,
      )
    }
  }
  console.log(
    plan.owed.length === 0
      ? `Nothing to pay; ${plan.paid} closed record${plan.paid === 1 ? " is" : "s are"} already paid`
      : `Would pay ${formatEther(plan.totalOwedWei)} GLM to ${plan.owed.length} provider${plan.owed.length === 1 ? "" : "s"}; ${plan.paid} already paid`,
  )

  const [glmWei, payoutGasWei, arkivGasWei] = await Promise.all([
    payout.glm(settle),
    payout.gas(settle),
    reader.balance(settle),
  ])
  console.log(
    `Paying from ${settle}: ${formatEther(glmWei)} GLM on chain ${payout.chain.id}, ${formatEther(payoutGasWei)} for gas there and ${formatEther(arkivGasWei)} on Arkiv for the receipts`,
  )
  const blockedBy = problems({ owedWei: plan.totalOwedWei, glmWei, payoutGasWei, arkivGasWei })
  for (const problem of blockedBy) {
    console.log(`Cannot pay: ${problem}`)
  }

  if (!run.paying) {
    console.log("Rehearsal: nothing was signed and nothing was written.")
    return
  }

  const writer = await connectWriter(arkivUrl, arkivKey, key as Hex)
  const paid = await pay({
    plan,
    chainId: payout.chain.id,
    transfer: payout.transfer,
    write: writer.write,
    log: (line) => console.log(line),
    blockedBy,
  })
  const total = paid.reduce((sum, entry) => sum + entry.amountWei, 0n)
  console.log(
    `Paid ${formatEther(total)} GLM to ${paid.length} of ${plan.owed.length} provider${plan.owed.length === 1 ? "" : "s"}`,
  )
  if (paid.length < plan.owed.length) {
    process.exitCode = 1
  }
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error))
  process.exit(1)
})
