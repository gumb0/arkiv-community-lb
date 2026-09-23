// What stands between a run and paying. Run: npm test

import { deepStrictEqual as deepEqual, strictEqual as equal, throws } from "node:assert/strict"
import { describe, it } from "node:test"
import { assertChain, problems } from "../src/payout.ts"

const GLM = 10n ** 18n

describe("what stops a run", () => {
  it("finds nothing wrong when the balances cover it", () => {
    deepEqual(problems({ owedWei: 5n * GLM, glmWei: 5n * GLM, payoutGasWei: 1n, arkivGasWei: 1n }), [])
  })

  it("reports a GLM balance that does not cover what is owed", () => {
    const [first, ...rest] = problems({ owedWei: 5n * GLM, glmWei: 4n * GLM, payoutGasWei: GLM, arkivGasWei: GLM })
    deepEqual(rest, [])
    deepEqual(first, "short of GLM: 5 owed, 4 held")
  })

  it("reports an empty gas balance on the payout chain", () => {
    deepEqual(problems({ owedWei: GLM, glmWei: GLM, payoutGasWei: 0n, arkivGasWei: GLM }), [
      "no gas on the payout chain, where the transfers are made",
    ])
  })

  it("reports an empty gas balance on Arkiv", () => {
    // The receipts are written there, and a run that pays without
    // them has paid a provider with nothing to say so.
    deepEqual(problems({ owedWei: GLM, glmWei: GLM, payoutGasWei: GLM, arkivGasWei: 0n }), [
      "no gas on Arkiv, where the receipts are written",
    ])
  })

  it("reports every one of them", () => {
    deepEqual(problems({ owedWei: GLM, glmWei: 0n, payoutGasWei: 0n, arkivGasWei: 0n }).length, 3)
  })

  it("is content with nothing owed, whatever is held", () => {
    // A run that would send nothing does not care what it holds.
    deepEqual(problems({ owedWei: 0n, glmWei: 0n, payoutGasWei: 0n, arkivGasWei: 0n }), [])
  })
})

describe("the chain the endpoint answers for", () => {
  it("passes when it is the one configured", () => {
    equal(assertChain(560048, 560048), 560048)
  })

  it("refuses any other, naming both", () => {
    throws(() => assertChain(1, 560048), /chain 1.*says 560048/)
  })
})
