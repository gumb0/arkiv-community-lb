// How the service and the probes read their configuration: variables
// from the environment, and the signing key from a file. The key is a
// file because a compose secret is a file, so the deployed key never sits
// in the container's environment where `docker inspect` would show it.

import { readFileSync } from "node:fs"
import type { Hex } from "viem"

/** A variable's value; a missing required one ends the process. */
export function env(name: string, required = true): string {
  const value = process.env[name] ?? ""
  if (required && !value) {
    console.error(`missing env var ${name} (see .env.example)`)
    process.exit(1)
  }
  return value
}

/**
 * The signing key from the file `WRITER_PRIVATE_KEY_FILE` names, trimmed
 * so an editor's final newline does not corrupt it. An empty file ends
 * the process, before the SDK could fail on it less clearly.
 */
export function privateKeyFromFile(): Hex {
  const path = env("WRITER_PRIVATE_KEY_FILE")
  const key = readFileSync(path, "utf8").trim()
  if (!key) {
    console.error(`the key file ${path} is empty`)
    process.exit(1)
  }
  return key as Hex
}
