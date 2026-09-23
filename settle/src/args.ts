// What a run was asked to do. The command line is small on purpose:
// a run pays every closed record that has no receipt, so there is
// nothing to select and nothing to schedule.

export type Run = { paying: boolean; help: boolean }

export const USAGE = `settle — pays each closed counter record the LB wrote

  npm run settle            rehearse: read, compute, print, write nothing
  npm run settle -- --pay   pay: one transfer per provider, then the receipts

A run pays every closed counter record that has no receipt yet, so
what it pays is decided by the chain and not by a flag. Rehearsing
needs no key.

Configuration comes from the environment; see .env.example.

Exit code 1 means the run refused to start, or that a provider it
meant to pay was not paid. Anything printed as PAID BUT NOT RECEIPTED
needs a person before the next run.`

/** Refuses what it does not understand rather than ignoring it. */
export function parseArgs(argv: readonly string[]): Run {
  const run: Run = { paying: false, help: false }
  for (const argument of argv) {
    switch (argument) {
      case "--pay":
        run.paying = true
        break
      // Rehearsing is what a run does when it is not told to pay. The
      // word is accepted so a cron line can say which it means.
      case "--rehearse":
        break
      case "--help":
      case "-h":
        run.help = true
        break
      default:
        throw new Error(`unknown argument ${argument}`)
    }
  }
  if (run.paying && argv.includes("--rehearse")) {
    throw new Error("--pay and --rehearse ask for different things")
  }
  return run
}
