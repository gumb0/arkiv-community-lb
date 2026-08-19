// Tests for the HTTP shell, driven by a fake writer — no chain, no key, no
// gas. What they cover is the shell's own contract: the wire format in both
// directions, which bodies are refused, how failures are reported, and that
// writes never overlap.
//
// The errors are the real ones (the SDK's EntityMutationError, viem's
// timeout error), so these break if either upstream changes the shape the
// shell reads. Run: npm test

import { EntityMutationError } from "@arkiv-network/sdk"
import { deepStrictEqual as deepEqual, ok, rejects, strictEqual as equal } from "node:assert/strict"
import { after, before, describe, it } from "node:test"
import { WaitForTransactionReceiptTimeoutError, type Hex } from "viem"
import { startService, type Service } from "../src/service.ts"
import type { Writer } from "../src/writer.ts"

const ADDRESS = "0xCA4B166EE155Cb2816Dc25f94Dc1fD102a26c997" as Hex
const TX_HASH = `0x${"ab".repeat(32)}` as Hex
const KEY_A = `0x${"11".repeat(32)}` as Hex
const KEY_B = `0x${"22".repeat(32)}` as Hex

type Call = { method: string; op: unknown }

/** A writer that records what it was asked to do and answers with canned results. */
function makeFake() {
  const calls: Call[] = []
  const state = { failWith: undefined as Error | undefined, delayMs: 0 }
  let inFlight = 0
  let maxInFlight = 0

  async function enter(method: string, op: unknown) {
    calls.push({ method, op })
    inFlight += 1
    maxInFlight = Math.max(maxInFlight, inFlight)
    try {
      if (state.delayMs > 0) await new Promise((done) => setTimeout(done, state.delayMs))
      if (state.failWith) throw state.failWith
    } finally {
      inFlight -= 1
    }
  }

  const writer: Writer = {
    address: ADDRESS,
    chainId: 7733102,
    createEntity: async (op) => {
      await enter("create", op)
      return { entityKey: KEY_A, txHash: TX_HASH, expiresAt: 1234n }
    },
    patchEntity: async (op) => {
      await enter("patch", op)
      return { entityKey: op.entityKey, txHash: TX_HASH }
    },
    deleteEntity: async (op) => {
      await enter("delete", op)
      return { entityKey: op.entityKey, txHash: TX_HASH }
    },
    extendEntity: async (op) => {
      await enter("extend", op)
      return { entityKey: op.entityKey, txHash: TX_HASH, expiresAt: 5678n }
    },
    mutateEntities: async (ops) => {
      await enter("mutate", ops)
      return {
        txHash: TX_HASH,
        createdEntities: [KEY_A, KEY_B],
        patchedEntities: [],
        deletedEntities: [],
        extendedEntities: [],
        ownershipChanges: [],
      }
    },
  }

  return { writer, calls, state, maxInFlight: () => maxInFlight }
}

let fake: ReturnType<typeof makeFake>
let service: Service
let base: string

before(async () => {
  fake = makeFake()
  service = await startService(fake.writer, { port: 0 })
  base = `http://${service.host}:${service.port}`
})

after(() => service.close())

async function post(route: string, body: unknown, raw?: string) {
  const response = await fetch(`${base}${route}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: raw ?? JSON.stringify(body),
  })
  return { status: response.status, body: (await response.json()) as Record<string, unknown> }
}

/** An indexed read that must have found something — asserts, then narrows. */
function at<T>(items: readonly T[], index: number, what: string): T {
  const item = items[index]
  ok(item !== undefined, `${what}[${index}] is missing`)
  return item
}

/** The op the fake was last handed, for the given method. */
function lastOp<T>(method: string): T {
  const call = [...fake.calls].reverse().find((entry) => entry.method === method)
  ok(call, `expected a ${method} call`)
  return call.op as T
}

const b64 = (text: string) => Buffer.from(text, "utf8").toString("base64")

const minimalCreate = {
  payload: b64("{}"),
  contentType: "application/json",
  expires: { seconds: 60 },
}

describe("wire format in", () => {
  it("decodes every attribute type into the SDK's tagged values", async () => {
    const { status } = await post("/create", {
      ...minimalCreate,
      attributes: {
        name: { type: "str", value: "provider-1" },
        rank: { type: "i32", value: -3 },
        slots: { type: "u64", value: "18446744073709551615" },
        big: { type: "u256", value: "123456789012345678901234567890" },
        price: { type: "dec", value: "1.25" },
        owner: { type: "addr", value: ADDRESS },
        parent: { type: "key", value: KEY_B },
        live: { type: "bool", value: true },
      },
    })
    equal(status, 200)
    const op = lastOp<{ attributes: Record<string, { type: string; value: unknown }> }>("create")
    deepEqual(op.attributes.name, { type: "str", value: "provider-1" })
    deepEqual(op.attributes.rank, { type: "i32", value: -3 })
    deepEqual(op.attributes.slots, { type: "u64", value: 18446744073709551615n })
    deepEqual(op.attributes.big, { type: "u256", value: 123456789012345678901234567890n })
    deepEqual(op.attributes.price, { type: "dec", value: "1.25" })
    deepEqual(op.attributes.owner, { type: "addr", value: ADDRESS })
    deepEqual(op.attributes.live, { type: "bool", value: true })
    deepEqual(op.attributes.parent, { type: "key", value: KEY_B })
  })

  it("decodes base64 payloads to the exact bytes", async () => {
    const payload = Buffer.from([0x00, 0xff, 0x10, 0x7f])
    await post("/create", { ...minimalCreate, payload: payload.toString("base64") })
    const op = lastOp<{ payload: Uint8Array }>("create")
    ok(op.payload instanceof Uint8Array)
    deepEqual([...op.payload], [0, 255, 16, 127])
  })

  it("converts each expiry form, seconds to whole blocks", async () => {
    await post("/create", { ...minimalCreate, expires: { seconds: 60 } })
    deepEqual(lastOp<{ expires: { minLifetime: bigint; expiresAt: bigint } }>("create").expires
      .minLifetime, 30n) // 2 s blocks

    await post("/create", { ...minimalCreate, expires: { blocks: 7 } })
    equal(lastOp<{ expires: { minLifetime: bigint } }>("create").expires.minLifetime, 7n)

    await post("/create", { ...minimalCreate, expires: { atBlock: "900000" } })
    equal(lastOp<{ expires: { expiresAt: bigint } }>("create").expires.expiresAt, 900000n)

    await post("/create", { ...minimalCreate, expires: "permanent" })
    equal(lastOp<{ expires: { expiresAt: bigint } }>("create").expires.expiresAt, 2n ** 64n - 1n)
  })

  it("passes creation flags and salt through", async () => {
    await post("/create", {
      ...minimalCreate,
      flags: { readonly: true },
      salt: "42",
    })
    const op = lastOp<{ flags: Record<string, boolean>; salt: bigint }>("create")
    deepEqual(op.flags, { readonly: true, permissionlessExtension: false })
    equal(op.salt, 42n)
  })

  it("decodes a patch's four independent parts", async () => {
    await post("/patch", {
      entityKey: KEY_A,
      set: { status: { type: "str", value: "dormant" } },
      unset: ["tag"],
      payload: b64("body"),
      contentType: "text/plain",
    })
    const op = lastOp<{
      entityKey: Hex
      set: Record<string, unknown>
      unset: string[]
      payload: Uint8Array
      contentType: string
    }>("patch")
    equal(op.entityKey, KEY_A)
    deepEqual(op.set.status, { type: "str", value: "dormant" })
    deepEqual(op.unset, ["tag"])
    equal(Buffer.from(op.payload).toString("utf8"), "body")
    equal(op.contentType, "text/plain")
  })

  it("keeps batch order and shape in /mutate", async () => {
    const { status } = await post("/mutate", {
      creates: [minimalCreate, { ...minimalCreate, contentType: "text/plain" }],
      patches: [{ entityKey: KEY_A, set: { a: { type: "i32", value: 1 } } }],
      deletes: [{ entityKey: KEY_B }],
      extensions: [{ entityKey: KEY_A, expires: { blocks: 10 } }],
    })
    equal(status, 200)
    const ops = lastOp<{
      creates: { contentType: string }[]
      patches: { entityKey: Hex; set: Record<string, unknown> }[]
      deletes: { entityKey: Hex }[]
      extensions: { entityKey: Hex; expires: { minLifetime: bigint } }[]
    }>("mutate")
    equal(ops.creates.length, 2)
    equal(at(ops.creates, 1, "creates").contentType, "text/plain") // order preserved

    equal(ops.patches.length, 1)
    const patch = at(ops.patches, 0, "patches")
    equal(patch.entityKey, KEY_A)
    deepEqual(patch.set.a, { type: "i32", value: 1 })

    equal(ops.deletes.length, 1)
    equal(at(ops.deletes, 0, "deletes").entityKey, KEY_B)

    equal(ops.extensions.length, 1)
    const extension = at(ops.extensions, 0, "extensions")
    equal(extension.entityKey, KEY_A)
    equal(extension.expires.minLifetime, 10n)
  })

  it("omits absent optional fields rather than sending empty ones", async () => {
    await post("/patch", { entityKey: KEY_A, set: { a: { type: "i32", value: 1 } } })
    const op = lastOp<Record<string, unknown>>("patch")
    ok(!("unset" in op), "unset stays absent when not given")
    ok(!("payload" in op), "payload stays absent when not given")
  })
})

describe("wire format out", () => {
  it("serializes bigints as decimal strings", async () => {
    const created = await post("/create", minimalCreate)
    equal(created.body.expiresAt, "1234")
    equal(created.body.entityKey, KEY_A)

    const extended = await post("/extend", { entityKey: KEY_A, expires: { blocks: 5 } })
    equal(extended.body.expiresAt, "5678")
  })

  it("returns the batch result arrays", async () => {
    const { body } = await post("/mutate", { deletes: [{ entityKey: KEY_A }] })
    deepEqual(body.createdEntities, [KEY_A, KEY_B])
    deepEqual(body.patchedEntities, [])
  })
})

describe("bad requests are refused before the chain", () => {
  const cases: [string, string, unknown, string][] = [
    ["unknown attribute type", "/create", { ...minimalCreate, attributes: { a: { type: "int", value: 1 } } }, "unknown type"],
    ["missing expires", "/create", { payload: b64("{}"), contentType: "application/json" }, "expires"],
    ["missing contentType", "/create", { payload: b64("{}"), expires: { seconds: 60 } }, "contentType"],
    ["expiry form that is not one of the four", "/create", { ...minimalCreate, expires: { years: 3 } }, "expires must be"],
    ["entity key without 0x", "/patch", { entityKey: "1234" }, "0x-prefixed"],
    ["unset that is not an array", "/patch", { entityKey: KEY_A, unset: "tag" }, "must be an array"],
    ["attribute value of the wrong JSON type", "/create", { ...minimalCreate, attributes: { a: { type: "i32", value: "5" } } }, "must be a number"],
    ["batch field that is not an array", "/mutate", { creates: { payload: "" } }, "must be an array"],
  ]

  for (const [name, route, body, expected] of cases) {
    it(name, async () => {
      const before = fake.calls.length
      const { status, body: result } = await post(route, body)
      equal(status, 400, `${name} should be 400`)
      const chain = result.error as { name: string; message: string }[]
      const head = at(chain, 0, "error chain")
      ok(head.message.includes(expected), `message names the problem: ${head.message}`)
      equal(fake.calls.length, before, "nothing reached the writer")
    })
  }

  it("malformed JSON", async () => {
    const { status } = await post("/create", undefined, "{not json")
    equal(status, 400)
  })

  it("a value out of its type's range, as the SDK sees it", async () => {
    // i32 bounds are the SDK's rule, not ours — the decoder calls its real
    // constructors, so validation happens here rather than on-chain.
    const { status, body } = await post("/create", {
      ...minimalCreate,
      attributes: { rank: { type: "i32", value: 2 ** 31 } },
    })
    equal(status, 400)
    equal(at(body.error as { name: string }[], 0, "error chain").name, "InvalidValueError")
  })
})

describe("routing", () => {
  it("unknown route is 404", async () => {
    const { status } = await post("/nope", {})
    equal(status, 404)
  })

  it("a GET on a real route is 404, not a write", async () => {
    const response = await fetch(`${base}/create`, { method: "GET" })
    equal(response.status, 404)
  })

  it("a body over the cap is 413 and never parsed", async () => {
    const before = fake.calls.length
    const response = await fetch(`${base}/create`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...minimalCreate, payload: "A".repeat(3 * 1024 * 1024) }),
    })
    equal(response.status, 413)
    equal(fake.calls.length, before, "nothing reached the writer")
  })
})

describe("startup", () => {
  it("rejects when the port is taken, rather than hanging", async () => {
    // The listener that carries this error to the caller is detached once the
    // port is bound, so this guards that the swap keeps the rejection. Without
    // it, a service that cannot bind would leave its caller waiting forever.
    await rejects(
      startService(makeFake().writer, { port: service.port }),
      (error: NodeJS.ErrnoException) => error.code === "EADDRINUSE",
    )
  })
})

describe("failures", () => {
  it("a failed write is 500 with the cause chain walked", async () => {
    const root = new Error("execution reverted")
    fake.state.failWith = new EntityMutationError("Transaction failed", { cause: root })
    const { status, body } = await post("/create", minimalCreate)
    fake.state.failWith = undefined

    equal(status, 500)
    const chain = body.error as { name: string; message: string }[]
    equal(at(chain, 0, "error chain").name, "EntityMutationError")
    equal(at(chain, 1, "error chain").message, "execution reverted")
  })

  it("a receipt timeout is 504 with the pending hash, not 500", async () => {
    // The hash lives only in viem's message; using the real error class means
    // this test fails if upstream rewords it, which is the point.
    const timeout = new WaitForTransactionReceiptTimeoutError({ hash: TX_HASH })
    fake.state.failWith = new EntityMutationError(`Transaction failed: ${timeout.message}`, {
      cause: timeout,
    })
    const { status, body } = await post("/create", minimalCreate)
    fake.state.failWith = undefined

    equal(status, 504, "sent-but-unconfirmed is its own outcome")
    deepEqual(body.pending, { txHash: TX_HASH })
  })
})

describe("serialization", () => {
  it("never lets two writes overlap", async () => {
    fake.state.delayMs = 25
    const results = await Promise.all([
      post("/create", minimalCreate),
      post("/create", minimalCreate),
      post("/mutate", { deletes: [{ entityKey: KEY_A }] }),
    ])
    fake.state.delayMs = 0

    for (const result of results) equal(result.status, 200)
    equal(fake.maxInFlight(), 1, "one write in flight at a time")
  })

  it("a failed write does not block the next one", async () => {
    fake.state.failWith = new EntityMutationError("boom")
    const failed = await post("/create", minimalCreate)
    fake.state.failWith = undefined
    const after = await post("/create", minimalCreate)

    equal(failed.status, 500)
    equal(after.status, 200, "the queue kept draining")
  })
})
