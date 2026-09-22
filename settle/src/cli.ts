// settle: pays each closed counter record the LB wrote. This is the
// rehearsal alone for now — it reads, computes and prints what it
// would pay. Nothing is signed and nothing is written.

import { formatEther, getAddress, type Hex } from "viem"
import { connectReader } from "./chain.ts"
import { ledger } from "./ledger.ts"

function required(name: string): string {
  const value = process.env[name]
  if (value === undefined || value === "") {
    throw new Error(`${name} is not set`)
  }
  return value
}

async function main(): Promise<void> {
  const reader = await connectReader(required("ARKIV_RPC_URL"), process.env.ARKIV_API_KEY)
  const lb = getAddress(required("LB_ADDRESS")) as Hex
  // Whose receipts count as paid. The providers' tooling reads
  // receipts by this address too, so a run with the wrong one would
  // pay records that are already paid.
  const settle = getAddress(required("SETTLE_ADDRESS")) as Hex

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
  console.log("Rehearsal: nothing was signed and nothing was written.")
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error))
  process.exit(1)
})
