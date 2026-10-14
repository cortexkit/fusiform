# Operations

Task-first guide for an operator. Every command here is `ck models <verb>`,
which runs the `ck-models` binary. It talks to the running module through the
supervisor; it never opens the database. Add `--json` to any command to print
the raw tool response instead of the summary, and `--subc <path>` to name a
connection file other than the per-user default (the `SUBC_CONNECTION_FILE`
environment variable also works).

The fields each command prints are defined in
[`wire-contract.md`](wire-contract.md).

## Is fusiform healthy and current?

```sh
ck models status
ck models status --polls 50
```

Read, in order:

1. `recent_polls`: the newest polls, including 304s and failures. A run of
   `not_modified` and `unchanged` is normal. A `changed` poll shows how many
   models arrived, were withdrawn, and how many facts changed.
2. `failures`: present only if a poll has ever failed. It shows the total, the
   last failure's class (`network`, `http_status`, `parse`, `implausible`) and
   how many polls back it is; pass that number to `--polls` to see it.
3. `history_begins_at_ms`: the earliest instant a point-in-time read accepts.

The supervisor's health report for the module (see
[`module-surface.md`](module-surface.md) §6) answers whether it is running and
fresh; `status` answers what it has been doing.

An `implausible` failure means the shrink guard refused a document that would
have removed more than a quarter of the catalog. Fusiform keeps serving the
previous catalog. Check the upstream; the guard clears on its own when a
normal document arrives.

## Read the catalog

```sh
ck models get --provider anthropic
ck models get --provider anthropic --model claude-sonnet-4-5
ck models get --provider anthropic --model claude-sonnet-4-5 --rates
ck models get --capabilities
ck models get --provider openai --model gpt-5 --at 1786500000000
ck models get --provider P --model M --include-retired
ck models get --provider openai --auth-method chatgpt
```

- `--provider` takes a models.dev provider slug. An unknown slug is refused
  rather than answered with zero models.
- `--model` requires `--provider`.
- `--rates` keeps only `rate.*` facts; `--capabilities` keeps `capability.*`
  and `limit.*`.
- `--at` takes epoch milliseconds and replays the catalog as it stood then.
  It is refused for an instant before `history_begins_at_ms`.
- `--auth-method` selects a billing plane and requires `--provider`; it cannot
  be combined with `--at`. See [`billing-planes.md`](billing-planes.md).

## See how one fact changed

```sh
ck models history --provider anthropic --model claude-sonnet-4-5 --fact rate.input
ck models history --provider anthropic --model claude-sonnet-4-5 --fact existence
```

Each era shows its value, its boundary instant, its boundary kind, and for an
observed change the window the change happened in. A `corrected` era shows the
correction's window and reason. An empty history says which cause applies: an
unknown model is refused, a rate served by inheritance or an alias names the
row whose history to ask for, and a curated row names its source.

## See subscription prices

```sh
ck models plan-prices
ck models plan-prices --provider openai
```

Each row shows its price or the reason it has none, its source, and its review
date; overdue rows are flagged. A provider with no curated rows is refused as
uncurated. See [`plan-prices.md`](plan-prices.md).

## Correct a fact fusiform recorded wrongly

Use this when a parser defect made fusiform record a wrong value for a past
window, the defect is fixed, and a poll since the fix has recorded the right
value. A correction makes reads inside the window withhold the fact; it never
changes a value.

1. Find the window. `affected_from` is the earliest plausible instant the bad
   value could have been recorded; when unsure, go earlier. `affected_until`
   is when the fix landed.
2. Preview:

   ```sh
   ck models correct --provider P --model M \
     --field rate.input --field rate.output \
     --from 1786000000000 --until 1786400000000 \
     --reason <finding>
   ```

   `--field` takes the fact key as `get` and `history` print it and may be
   repeated. `--reason` names the finding document: the written defect report
   that explains what was wrong and when, for example its path or URL.
   The preview lists each fact and the value that stays in force.
3. If it is refused, read the message. "Poll first" means the value in force
   was itself recorded inside the window: wait for the next poll (or for the
   fixed parser to run), then retry.
4. Commit by adding `--commit` to the same command.

Verify:

```sh
ck models get --provider P --model M --at <an instant inside the window> --json
```

The fact is absent from `models` and named under `withheld` with the
correction. `ck models history` shows a `corrected` era.

Reverse: there is no delete. A correction recorded in error stays on the
record; tell consumers which correction it was. Corrections cannot reach past
the instant they were recorded, so a read of the present is never withheld.

## Mark a poll as a representation change

Use this when a poll's eras record fusiform changing its own representation of
values (for example a serialization change) rather than the upstream changing
its data. The mark stops `last_changed_at_ms` in `catalog.history` from
reporting that poll as a change. It changes no stored value.

1. Find the observation id with `ck models status --polls N --json`.
2. Preview:

   ```sh
   ck models mark-artifact --observation 1234 --reason <evidence>
   ```

   The preview shows the poll's instant, how many eras it wrote, and how many
   restate the claim already in force. Only the restating eras are excluded;
   a genuine change in the same poll still counts.
3. Commit by adding `--commit`.

Verify: `ck models history` for an affected fact shows a `last_changed_at_ms`
earlier than the marked poll.

Reverse:

```sh
ck models retract-artifact --observation 1234 --reason <why> --commit
```

The retraction is appended after the mark; both stay on the record. The
preview reports whether there was a live mark to take back.

## A review gate failed in CI

A test failing with "NOT A BUILD FAILURE" or "past its review date" means a
curated claim's review date passed or the embedded seed aged past its limit.
Nothing in the code changed. Follow [`curated-data.md`](curated-data.md) §6.

## Check which build is running

```sh
ck-fusiform --version
ck-fusiform --manifest
```

`--version` prints the binary version, the build revision (`unknown` for an
unstamped development build) and the wire schema version. `--manifest` prints
the registration manifest, including the build provenance. Neither needs a
running daemon.
