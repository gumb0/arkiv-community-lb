// HTTP shell for the chain-writer sidecar: JSON routes over the schema-blind
// writer module (see AGENTS.md — the sidecar speaks SDK, not marketplace).
// Meant for the compose-internal network only; never expose it publicly.
//
// Writes are serialized: one transaction in flight at a time, later requests
// wait their turn. Batching stays the caller's job — send one /mutate body
// instead of many single-op requests when atomicity or cost matters.
//
// Wire format (all routes are POST, bodies and responses are JSON):
//   payload      base64 string
//   entity keys  0x-prefixed hex strings
//   attributes   { name: { type: "str"|"bool"|"i32"|"u64"|"u256"|"dec"|
//                          "addr"|"key"|"bytes32", value: ... } }
//                (u64/u256/dec/salt values are decimal strings, since JSON
//                numbers cannot carry them)
//   expires      "permanent" | { seconds: n } | { blocks: n }
//                | { atBlock: "decimal string" }
//   responses    the SDK's return shape, with bigints as decimal strings
// Errors: 400 for a body that does not decode, 500 for a failed write, and
// 504 for a write whose receipt did not arrive in time — the transaction may
// still be mined, so 504 means "unresolved", never "retry me". All three carry
// { error: [{ name, message }, ...] } — the walked cause chain; 504 also
// carries { pending: { txHash } } so the caller can settle it by polling.

import {
  addr,
  type AttributeInputs,
  bool,
  bytes32,
  dec,
  ExpirationTime,
  type Expiry,
  i32,
  key,
  str,
  u64,
  u256,
  type ValueInput,
} from "@arkiv-network/sdk"
import { createServer, type Server } from "node:http"
import { pathToFileURL } from "node:url"
import type { Hex } from "viem"
import {
  type BatchOps,
  type CreateOp,
  createWriter,
  type DeleteOp,
  type ExtendOp,
  type PatchOp,
  type Writer,
} from "./writer.ts"

/** A request body that does not decode — reported as 400, never queued. */
class DecodeError extends Error {}

function fail(message: string): never {
  throw new DecodeError(message)
}

function asRecord(wire: unknown, what: string): Record<string, unknown> {
  if (typeof wire !== "object" || wire === null || Array.isArray(wire)) {
    fail(`${what} must be a JSON object`)
  }
  return wire as Record<string, unknown>
}

function asString(value: unknown, what: string): string {
  if (typeof value !== "string") fail(`${what} must be a string`)
  return value
}

function asHex(value: unknown, what: string): Hex {
  const s = asString(value, what)
  if (!s.startsWith("0x")) fail(`${what} must be 0x-prefixed hex`)
  return s as Hex
}

function decodeValue(wire: unknown, name: string): ValueInput {
  const record = asRecord(wire, `attribute ${name}`)
  const value = record.value
  switch (record.type) {
    case "str":
      return str(asString(value, `attribute ${name}`))
    case "bool":
      if (typeof value !== "boolean") fail(`attribute ${name} must be a boolean`)
      return bool(value)
    case "i32":
      if (typeof value !== "number") fail(`attribute ${name} must be a number`)
      return i32(value)
    case "u64":
      return u64(asString(value, `attribute ${name}`))
    case "u256":
      return u256(asString(value, `attribute ${name}`))
    case "dec":
      return dec(asString(value, `attribute ${name}`))
    case "addr":
      return addr(asString(value, `attribute ${name}`))
    case "key":
      return key(asHex(value, `attribute ${name}`))
    case "bytes32":
      return bytes32(asHex(value, `attribute ${name}`))
    default:
      fail(`attribute ${name} has unknown type ${JSON.stringify(record.type)}`)
  }
}

function decodeAttributes(wire: unknown, what: string): AttributeInputs {
  const record = asRecord(wire, what)
  return Object.fromEntries(
    Object.entries(record).map(([name, value]) => [name, decodeValue(value, name)]),
  )
}

function decodeExpiry(wire: unknown): Expiry {
  if (wire === "permanent") return ExpirationTime.permanent()
  const record = asRecord(wire, "expires")
  if (typeof record.seconds === "number") return ExpirationTime.fromSeconds(record.seconds)
  if (typeof record.blocks === "number") return ExpirationTime.fromBlocks(record.blocks)
  if (typeof record.atBlock === "string") return ExpirationTime.atBlock(BigInt(record.atBlock))
  fail(`expires must be "permanent", { seconds }, { blocks } or { atBlock }`)
}

function decodePayload(wire: unknown, what: string): Uint8Array {
  return Uint8Array.from(Buffer.from(asString(wire, what), "base64"))
}

function decodeCreate(wire: unknown): CreateOp {
  const record = asRecord(wire, "create")
  const op: CreateOp = {
    payload: decodePayload(record.payload, "create.payload"),
    contentType: asString(record.contentType, "create.contentType"),
    expires: decodeExpiry(record.expires),
  }
  if (record.attributes !== undefined) {
    op.attributes = decodeAttributes(record.attributes, "create.attributes")
  }
  if (record.flags !== undefined) {
    const flags = asRecord(record.flags, "create.flags")
    op.flags = {
      readonly: flags.readonly === true,
      permissionlessExtension: flags.permissionlessExtension === true,
    }
  }
  if (record.salt !== undefined) op.salt = BigInt(asString(record.salt, "create.salt"))
  return op
}

function decodePatch(wire: unknown): PatchOp {
  const record = asRecord(wire, "patch")
  const op: PatchOp = { entityKey: asHex(record.entityKey, "patch.entityKey") }
  if (record.set !== undefined) op.set = decodeAttributes(record.set, "patch.set")
  if (record.unset !== undefined) {
    if (!Array.isArray(record.unset)) fail("patch.unset must be an array")
    op.unset = record.unset.map((name, i) => asString(name, `patch.unset[${i}]`))
  }
  if (record.payload !== undefined) op.payload = decodePayload(record.payload, "patch.payload")
  if (record.contentType !== undefined) {
    op.contentType = asString(record.contentType, "patch.contentType")
  }
  return op
}

function decodeDelete(wire: unknown): DeleteOp {
  return { entityKey: asHex(asRecord(wire, "delete").entityKey, "delete.entityKey") }
}

function decodeExtend(wire: unknown): ExtendOp {
  const record = asRecord(wire, "extend")
  return {
    entityKey: asHex(record.entityKey, "extend.entityKey"),
    expires: decodeExpiry(record.expires),
  }
}

function decodeBatch(wire: unknown): BatchOps {
  const record = asRecord(wire, "mutate")
  const list = (field: unknown, what: string): unknown[] => {
    if (!Array.isArray(field)) fail(`${what} must be an array`)
    return field
  }
  const ops: BatchOps = {}
  if (record.creates !== undefined) ops.creates = list(record.creates, "creates").map(decodeCreate)
  if (record.patches !== undefined) ops.patches = list(record.patches, "patches").map(decodePatch)
  if (record.deletes !== undefined) ops.deletes = list(record.deletes, "deletes").map(decodeDelete)
  if (record.extensions !== undefined) {
    ops.extensions = list(record.extensions, "extensions").map(decodeExtend)
  }
  return ops
}

function errorChain(error: unknown): { name: string; message: string }[] {
  const chain: { name: string; message: string }[] = []
  let current: unknown = error
  while (current instanceof Error && chain.length < 5) {
    chain.push({ name: current.constructor.name, message: current.message })
    current = current.cause
  }
  if (chain.length === 0) chain.push({ name: "Unknown", message: String(error) })
  return chain
}

/**
 * The hash of a transaction that was sent but whose receipt never arrived
 * (viem gives up after 180 s and the SDK does not expose that timeout).
 *
 * The hash only exists in the error's message — neither `EntityMutationError`
 * nor viem's timeout error carries it as a field — so this reads it back out.
 * Message parsing is the fallback tier by design, and this is the case it is
 * for: a transport-level outcome with no typed carrier.
 */
function pendingTxHash(chain: { name: string; message: string }[]): Hex | undefined {
  const timedOut = chain.find((link) => link.name === "WaitForTransactionReceiptTimeoutError")
  const hash = timedOut?.message.match(/0x[0-9a-fA-F]{64}/)?.[0]
  return hash as Hex | undefined
}

const MAX_BODY_BYTES = 2 * 1024 * 1024

export type ServiceOptions = {
  /** Default 127.0.0.1 — bind wider only inside a private compose network. */
  host?: string
  /** Default 8560; 0 picks an ephemeral port (used by the smoke test). */
  port?: number
}

export type Service = { host: string; port: number; close: () => Promise<void> }

export function startService(writer: Writer, options: ServiceOptions = {}): Promise<Service> {
  const host = options.host ?? "127.0.0.1"
  const port = options.port ?? 8560

  // One write in flight at a time; a failed write never blocks the next.
  let tail: Promise<unknown> = Promise.resolve()
  const enqueue = <T>(work: () => Promise<T>): Promise<T> => {
    const result = tail.then(work)
    tail = result.catch(() => undefined)
    return result
  }

  // Decoding and writing are kept apart so the status code follows from
  // *where* a failure happened, not from the error's class: anything thrown
  // while decoding is the caller's bad request, anything thrown by the writer
  // is the chain's answer. The SDK's own validators (i32 range, attribute
  // names, expiry shape) run inside the decoders, so their errors land on the
  // 400 side without this having to know their names.
  const route = <T>(decode: (body: unknown) => T, write: (op: T) => Promise<unknown>) =>
    (body: unknown) => {
      const op = decode(body)
      return () => write(op)
    }

  const routes: Record<string, (body: unknown) => () => Promise<unknown>> = {
    "/create": route(decodeCreate, (op) => writer.createEntity(op)),
    "/patch": route(decodePatch, (op) => writer.patchEntity(op)),
    "/delete": route(decodeDelete, (op) => writer.deleteEntity(op)),
    "/extend": route(decodeExtend, (op) => writer.extendEntity(op)),
    "/mutate": route(decodeBatch, (ops) => writer.mutateEntities(ops)),
  }

  const server: Server = createServer((request, response) => {
    const respond = (status: number, body: unknown, sent?: () => void) => {
      response.writeHead(status, { "content-type": "application/json" })
      response.end(
        JSON.stringify(body, (_k, v) => (typeof v === "bigint" ? v.toString() : v)),
        sent,
      )
    }
    const respondError = (status: number, error: unknown) => {
      const chain = errorChain(error)
      const pending = pendingTxHash(chain)
      if (pending) return respond(504, { error: chain, pending: { txHash: pending } })
      respond(status, { error: chain })
    }

    const route = routes[request.url ?? ""]
    if (request.method !== "POST" || !route) {
      return respondError(404, new Error(`no POST route ${request.url}`))
    }

    const chunks: Buffer[] = []
    let size = 0
    let refused = false
    request.on("data", (chunk: Buffer) => {
      if (refused) return
      size += chunk.length
      if (size > MAX_BODY_BYTES) {
        // Stop buffering, but keep reading to the end before answering.
        // Replying mid-upload races the caller's own writes: it sees the
        // socket close, reads that as a transport failure, and retries a body
        // that will never fit. This listener is compose-internal, so draining
        // a runaway body is the cheaper problem.
        refused = true
        chunks.length = 0
        return
      }
      chunks.push(chunk)
    })
    request.on("end", async () => {
      if (refused) {
        return respond(413, {
          error: errorChain(new Error(`body over ${MAX_BODY_BYTES} bytes`)),
        })
      }
      let write: () => Promise<unknown>
      try {
        write = route(JSON.parse(Buffer.concat(chunks).toString("utf8")))
      } catch (error) {
        return respondError(400, error)
      }
      try {
        respond(200, await enqueue(write))
      } catch (error) {
        respondError(500, error)
      }
    })
  })

  return new Promise((resolve, reject) => {
    server.once("error", reject)
    server.listen(port, host, () => {
      const address = server.address()
      const boundPort = typeof address === "object" && address ? address.port : port
      resolve({
        host,
        port: boundPort,
        close: () => new Promise((done) => server.close(() => done())),
      })
    })
  })
}

// Run directly: read config from the environment and serve until signalled.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const env = (name: string, required = true): string => {
    const value = process.env[name] ?? ""
    if (required && !value) {
      console.error(`missing env var ${name} (see .env.example)`)
      process.exit(1)
    }
    return value
  }

  const writer = await createWriter({
    rpcUrl: env("RPC_URL").replace(/\/+$/, ""),
    apiKey: env("API_KEY", false) || undefined,
    privateKey: env("PRIVATE_KEY") as Hex,
  })
  const service = await startService(writer, {
    host: env("HOST", false) || undefined,
    port: env("PORT", false) ? Number(env("PORT", false)) : undefined,
  })
  console.log(
    `chain-writer service: ${service.host}:${service.port}, ` +
      `address ${writer.address}, chain ${writer.chainId}`,
  )
  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    process.on(signal, async () => {
      await service.close()
      process.exit(0)
    })
  }
}
