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
    executeBatch: async (ops) => {
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

const minimalCreate = {
  attributes: { kind: { type: "str", value: "probe" } },
  expiration: { seconds: 60 },
}

/** What a create or a patch declared and carries, as the fake saw it. */
type Declared = {
  schema: {
    name: string | undefined
    columns: Record<string, { type: string; isIndexed: boolean; isOptional: boolean }>
  }
  values: Record<string, { type: string; value: unknown }>
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
        blob: { type: "bytes", value: "0x00ff107f" },
      },
    })
    equal(status, 200)
    const op = lastOp<Declared>("create")
    deepEqual(op.values.name, { type: "str", value: "provider-1" })
    deepEqual(op.values.rank, { type: "i32", value: -3 })
    deepEqual(op.values.slots, { type: "u64", value: 18446744073709551615n })
    deepEqual(op.values.big, { type: "u256", value: 123456789012345678901234567890n })
    deepEqual(op.values.price, { type: "dec", value: "1.25" })
    deepEqual(op.values.owner, { type: "addr", value: ADDRESS })
    deepEqual(op.values.live, { type: "bool", value: true })
    deepEqual(op.values.parent, { type: "key", value: KEY_B })
    deepEqual(op.values.blob, { type: "bytes", value: "0x00ff107f" })
  })

  it("declares the attributes as an unnamed schema, indexed unless said otherwise", async () => {
    await post("/create", {
      ...minimalCreate,
      attributes: {
        kind: { type: "str", value: "probe" },
        note: { type: "str", value: "stored only", indexed: false },
        rank: { type: "i32", value: 1, indexed: true },
        blob: { type: "bytes", value: "0x00" },
      },
    })
    const { schema } = lastOp<Declared>("create")
    equal(schema.name, undefined, "no arkiv.schema tag")
    deepEqual(Object.keys(schema.columns).sort(), ["blob", "kind", "note", "rank"])
    equal(schema.columns.kind?.isIndexed, true)
    equal(schema.columns.note?.isIndexed, false)
    equal(schema.columns.rank?.isIndexed, true)
    equal(schema.columns.blob?.isIndexed, false)
    equal(schema.columns.blob?.type, "bytes")
  })

  it("converts each expiry form, seconds to whole blocks", async () => {
    type Expiring = { expiration: { minLifetime: bigint; expiresAt: bigint } }
    await post("/create", { ...minimalCreate, expiration: { seconds: 60 } })
    equal(lastOp<Expiring>("create").expiration.minLifetime, 30n) // 2 s blocks

    await post("/create", { ...minimalCreate, expiration: { blocks: 7 } })
    equal(lastOp<Expiring>("create").expiration.minLifetime, 7n)

    await post("/create", { ...minimalCreate, expiration: { atBlock: "900000" } })
    equal(lastOp<Expiring>("create").expiration.expiresAt, 900000n)
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

  it("decodes a patch's set and unset, and declares both as columns", async () => {
    await post("/patch", {
      entityKey: KEY_A,
      set: { status: { type: "str", value: "dormant" }, body: { type: "bytes", value: "0x01" } },
      unset: ["tag"],
    })
    const op = lastOp<
      { entityKey: Hex; set: Record<string, unknown>; unset: string[] } & Declared
    >("patch")
    equal(op.entityKey, KEY_A)
    deepEqual(op.set.status, { type: "str", value: "dormant" })
    deepEqual(op.set.body, { type: "bytes", value: "0x01" })
    deepEqual(op.unset, ["tag"])
    deepEqual(Object.keys(op.schema.columns).sort(), ["body", "status", "tag"])
    equal(op.schema.columns.status?.isIndexed, true)
    equal(op.schema.columns.body?.isIndexed, false)
    // The SDK refuses to unset a required column, so the one declared
    // for an unset name must be optional.
    equal(op.schema.columns.tag?.isOptional, true)
    equal(op.schema.columns.status?.isOptional, false)
  })

  it("keeps batch order and shape in /execute-batch", async () => {
    const second = { ...minimalCreate, attributes: { kind: { type: "str", value: "second" } } }
    const { status } = await post("/execute-batch", {
      creates: [minimalCreate, second],
      patches: [{ entityKey: KEY_A, set: { a: { type: "i32", value: 1 } } }],
      deletes: [{ entityKey: KEY_B }],
      extensions: [{ entityKey: KEY_A, expiration: { blocks: 10 } }],
    })
    equal(status, 200)
    const ops = lastOp<{
      creates: Declared[]
      patches: { entityKey: Hex; set: Record<string, unknown> }[]
      deletes: { entityKey: Hex }[]
      extensions: { entityKey: Hex; expiration: { minLifetime: bigint } }[]
    }>("mutate")
    equal(ops.creates.length, 2)
    equal(at(ops.creates, 1, "creates").values.kind?.value, "second") // order preserved

    equal(ops.patches.length, 1)
    const patch = at(ops.patches, 0, "patches")
    equal(patch.entityKey, KEY_A)
    deepEqual(patch.set.a, { type: "i32", value: 1 })

    equal(ops.deletes.length, 1)
    equal(at(ops.deletes, 0, "deletes").entityKey, KEY_B)

    equal(ops.extensions.length, 1)
    const extension = at(ops.extensions, 0, "extensions")
    equal(extension.entityKey, KEY_A)
    equal(extension.expiration.minLifetime, 10n)
  })

  it("omits absent optional fields rather than sending empty ones", async () => {
    await post("/patch", { entityKey: KEY_A, set: { a: { type: "i32", value: 1 } } })
    const op = lastOp<Record<string, unknown>>("patch")
    ok(!("unset" in op), "unset stays absent when not given")
    await post("/patch", { entityKey: KEY_A, unset: ["a"] })
    ok(!("set" in lastOp<Record<string, unknown>>("patch")), "set stays absent when not given")
  })
})

describe("wire format out", () => {
  it("serializes bigints as decimal strings", async () => {
    const created = await post("/create", minimalCreate)
    equal(created.body.expiresAt, "1234")
    equal(created.body.entityKey, KEY_A)

    const extended = await post("/extend", { entityKey: KEY_A, expiration: { blocks: 5 } })
    equal(extended.body.expiresAt, "5678")
  })

  it("returns the batch result arrays", async () => {
    const { body } = await post("/execute-batch", { deletes: [{ entityKey: KEY_A }] })
    deepEqual(body.createdEntities, [KEY_A, KEY_B])
    deepEqual(body.patchedEntities, [])
  })
})

describe("bad requests are refused before the chain", () => {
  const cases: [string, string, unknown, string][] = [
    ["unknown attribute type", "/create", { ...minimalCreate, attributes: { a: { type: "int", value: 1 } } }, "unknown type"],
    ["the old bytes32 type", "/create", { ...minimalCreate, attributes: { a: { type: "bytes32", value: KEY_A } } }, "unknown type"],
    ["missing attributes", "/create", { expiration: { seconds: 60 } }, "attributes"],
    ["missing expiration", "/create", { attributes: minimalCreate.attributes }, "expiration"],
    ["expiry form that is not one of the three", "/create", { ...minimalCreate, expiration: { years: 3 } }, "expiration must be"],
    ["a permanent expiry, which the chain refuses", "/create", { ...minimalCreate, expiration: "permanent" }, "expiration must be"],
    ["bytes asked to be indexed", "/create", { ...minimalCreate, attributes: { b: { type: "bytes", value: "0x00", indexed: true } } }, "never indexed"],
    ["indexed that is not a boolean", "/create", { ...minimalCreate, attributes: { a: { type: "str", value: "x", indexed: "yes" } } }, "must be a boolean"],
    ["bytes that are not hex", "/create", { ...minimalCreate, attributes: { b: { type: "bytes", value: "AAA=" } } }, "0x-prefixed"],
    ["entity key without 0x", "/patch", { entityKey: "1234" }, "0x-prefixed"],
    ["a patch with nothing to apply", "/patch", { entityKey: KEY_A }, "set or unset"],
    ["unset that is not an array", "/patch", { entityKey: KEY_A, unset: "tag" }, "must be an array"],
    ["attribute value of the wrong JSON type", "/create", { ...minimalCreate, attributes: { a: { type: "i32", value: "5" } } }, "must be a number"],
    ["batch field that is not an array", "/execute-batch", { creates: { attributes: {} } }, "must be an array"],
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
      body: JSON.stringify({
        ...minimalCreate,
        attributes: { blob: { type: "bytes", value: `0x${"00".repeat(1536 * 1024)}` } },
      }),
    })
    equal(response.status, 413)
    equal(fake.calls.length, before, "nothing reached the writer")
  })
})

describe("identity", () => {
  it("GET /identity tells the signing address and the chain", async () => {
    const response = await fetch(`${base}/identity`)
    equal(response.status, 200)
    deepEqual(await response.json(), { address: ADDRESS, chainId: 7733102 })
  })

  it("a POST on /identity is 404", async () => {
    const { status } = await post("/identity", {})
    equal(status, 404)
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

  it("a transaction the node calls oversized is 413 with the node's words", async () => {
    // Built the way viem builds it: the node's message in `details` on
    // every link, the summary in `message`.
    const details = "oversized data: transaction size 131509, limit 131072"
    const rpc = Object.assign(new Error("Missing or invalid parameters."), { details })
    fake.state.failWith = new EntityMutationError("Transaction failed", { cause: rpc })
    const { status, body } = await post("/execute-batch", { creates: [minimalCreate] })
    fake.state.failWith = undefined

    equal(status, 413)
    const chain = body.error as { name: string; message: string; details?: string }[]
    equal(at(chain, 0, "error chain").details, undefined)
    equal(at(chain, 1, "error chain").details, details)
  })

  it("a receipt timeout is 504 with the pending hash, not 500", async () => {
    // Built the way the SDK builds it: viem's timeout as the cause, the
    // hash as a field.
    const timeout = new WaitForTransactionReceiptTimeoutError({ hash: TX_HASH })
    fake.state.failWith = new EntityMutationError(`Transaction failed: ${timeout.message}`, {
      cause: timeout,
      txHash: TX_HASH,
    })
    const { status, body } = await post("/create", minimalCreate)
    fake.state.failWith = undefined

    equal(status, 504, "sent-but-unconfirmed is its own outcome")
    deepEqual(body.pending, { txHash: TX_HASH })
  })
})

describe("client aborts do not kill the service", () => {
  // Today's Node handles both abort shapes without help (verified: these
  // pass with no error listeners in the service). They stay as a contract:
  // a refactor that touches the request/response streams — piping the body,
  // manual socket work — must not make a vanishing caller fatal.
  it("survives a connection reset in the middle of a request body", async () => {
    // Announce a large body, send half of it, and reset the socket.
    const { connect } = await import("node:net")
    await new Promise<void>((resolve) => {
      const socket = connect(service.port, service.host, () => {
        socket.write(
          "POST /create HTTP/1.1\r\n" +
            `Host: ${service.host}\r\n` +
            "Content-Type: application/json\r\n" +
            "Content-Length: 1000\r\n\r\n" +
            '{"attributes":',
          () => socket.destroy(),
        )
      })
      socket.on("close", resolve)
      socket.on("error", () => {})
    })
    // The service still answers.
    const { status } = await post("/create", minimalCreate)
    equal(status, 200)
  })

  it("survives the caller giving up while its write is still queued", async () => {
    fake.state.delayMs = 50
    const aborter = new AbortController()
    const doomed = fetch(`${base}/create`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(minimalCreate),
      signal: aborter.signal,
    }).catch(() => "aborted")
    setTimeout(() => aborter.abort(), 10)

    equal(await doomed, "aborted")
    // Give the queued write time to finish and respond into the closed
    // connection — the part that must not crash — then prove liveness.
    await new Promise((resolve) => setTimeout(resolve, 100))
    fake.state.delayMs = 0
    const { status } = await post("/create", minimalCreate)
    equal(status, 200)
  })
})

describe("serialization", () => {
  it("never lets two writes overlap", async () => {
    fake.state.delayMs = 25
    const results = await Promise.all([
      post("/create", minimalCreate),
      post("/create", minimalCreate),
      post("/execute-batch", { deletes: [{ entityKey: KEY_A }] }),
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
