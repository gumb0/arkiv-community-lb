// The payout chain: where the GLM lives and the transfers are made.
// Nothing here names Hoodi or Polygon — the chain id, the endpoint and
// the token are configuration, so a handover is a value change.

import {
  createPublicClient,
  createWalletClient,
  defineChain,
  encodeFunctionData,
  erc20Abi,
  formatEther,
  http,
  keccak256,
  type Chain,
  type Hex,
} from "viem"
import { privateKeyToAccount } from "viem/accounts"
import { Unresolved } from "./sent.ts"

export type PayoutConfig = {
  rpcUrl: string
  /** What the endpoint must answer with, so a run cannot pay on the wrong chain. */
  chainId: number
  /** The GLM contract on that chain. */
  token: Hex
  /** Absent while rehearsing: without it a run cannot send anything. */
  privateKey?: Hex
}

export type Payout = {
  chain: Chain
  /** The address the key gives, when a run has one. */
  address?: Hex
  /** GLM held by an address, in wei. */
  glm(address: Hex): Promise<bigint>
  /** The chain's own currency, for gas. */
  gas(address: Hex): Promise<bigint>
  /**
   * Sends GLM and waits for the transaction to be mined, so a run
   * writes a receipt for a payment that has landed rather than for
   * one that was sent. A transaction that reverts throws.
   */
  transfer(to: Hex, amountWei: bigint): Promise<Hex>
}

export async function connectPayout(config: PayoutConfig): Promise<Payout> {
  const client = createPublicClient({ transport: http(config.rpcUrl) })
  const chainId = assertChain(await client.getChainId(), config.chainId)
  const chain = defineChain({
    id: chainId,
    name: `payout-${chainId}`,
    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: [config.rpcUrl] } },
  })
  const account = config.privateKey === undefined ? undefined : privateKeyToAccount(config.privateKey)
  const wallet =
    account === undefined ? undefined : createWalletClient({ chain, transport: http(config.rpcUrl), account })

  return {
    chain,
    address: account?.address,
    transfer: async (to, amountWei) => {
      if (wallet === undefined || account === undefined) {
        throw new Error("no payout key: this run cannot send anything")
      }
      // Signed here, not by the node, so the hash is known before the
      // transaction leaves. Left to the node, a send whose reply is
      // lost would come back as a failure with no hash, and read as
      // "nothing was sent" for a transfer that may have been.
      const request = await wallet.prepareTransactionRequest({
        account,
        chain,
        to: config.token,
        data: encodeFunctionData({ abi: erc20Abi, functionName: "transfer", args: [to, amountWei] }),
      })
      const signed = await wallet.signTransaction(request)
      const hash = keccak256(signed)
      return broadcast(hash, async () => {
        await client.sendRawTransaction({ serializedTransaction: signed })
        return client.waitForTransactionReceipt({ hash })
      })
    },
    glm: (address) =>
      client.readContract({
        address: config.token,
        abi: erc20Abi,
        functionName: "balanceOf",
        args: [address],
      }),
    gas: (address) => client.getBalance({ address }),
  }
}

/**
 * Sends a signed transaction and waits for it to be mined. From the
 * broadcast on, every failure carries the hash: the send's reply can be
 * lost with the transaction accepted, the endpoint can go away while
 * waiting, and viem gives up after a few minutes of a chain that is
 * merely slow, and in each case the transfer may still be mined. A
 * reverted transaction moved nothing and is a plain failure.
 */
export async function broadcast(hash: Hex, send: () => Promise<{ status: string }>): Promise<Hex> {
  let receipt
  try {
    receipt = await send()
  } catch (error) {
    throw new Unresolved(hash, error)
  }
  if (receipt.status !== "success") {
    throw new Error(`the transfer ${hash} reverted, and nothing moved`)
  }
  return hash
}

/**
 * The chain the endpoint answers for, refused unless it is the one
 * configured: an endpoint pointed at the wrong chain would send the
 * transfers somewhere nobody is watching, and there is no taking them
 * back.
 */
export function assertChain(answered: number, expected: number): number {
  if (answered !== expected) {
    throw new Error(
      `the payout endpoint answers for chain ${answered}, and PAYOUT_CHAIN_ID says ${expected}`,
    )
  }
  return answered
}

/**
 * What stands between a run and paying, in the words a run prints. A
 * run spends on two chains: the transfers on the payout chain, the
 * receipts that record them on Arkiv. Both balances are checked
 * before anything is signed, because a run that stops between a
 * transfer and its receipt has paid a provider with nothing to say
 * so, and the next run would pay again.
 */
export function problems(held: {
  owedWei: bigint
  glmWei: bigint
  payoutGasWei: bigint
  arkivGasWei: bigint
}): string[] {
  // Nothing owed, nothing in the way: a run that would send nothing
  // does not care what it holds.
  if (held.owedWei === 0n) return []
  const found: string[] = []
  if (held.owedWei > held.glmWei) {
    found.push(
      `short of GLM: ${formatEther(held.owedWei)} owed, ${formatEther(held.glmWei)} held`,
    )
  }
  if (held.payoutGasWei === 0n) {
    found.push("no gas on the payout chain, where the transfers are made")
  }
  if (held.arkivGasWei === 0n) {
    found.push("no gas on Arkiv, where the receipts are written")
  }
  return found
}
