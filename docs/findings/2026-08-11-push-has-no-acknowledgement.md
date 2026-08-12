> **RESOLVED 2026-08-12, and not the way this document expected.**
>
> The delivery half was fixed: `subc-client-rs` 0.3.0 surfaces push frames to
> consumers, opt-in per route and epoch-fenced, plus an always-present
> `pushes_dropped_no_receiver` counter so a producer pushing into a discarding
> client can tell.
>
> The acknowledgement half was WITHDRAWN rather than built. Asking for the
> transport forced the question of what the acknowledgement was for, and the
> answer did not survive it: if push is the only notification path, an ack
> reports a staleness it cannot repair. With consumers keeping a slow poll
> backstop, a dropped push costs latency only — and the backstop is a better
> acknowledgement than a message, because it cannot itself be dropped.
>
> So the reduced ask was delivery plus visibility, with no wire change and no
> module-initiated request frame. Fusiform emits no pushes today; `emits_push`
> is false in its manifest.
>
> The finding below stands as measured. Keeping it because the DIAGNOSIS was
> right and the REMEDY it implied was wrong, which is the more useful half: a
> missing transport is easier to notice than an unnecessary requirement, and
> fixing the first would have closed the gap while preserving the mistake.

# The push surface fusiform specified cannot be built as specified

Measured 2026-08-11 against `subconscious@a23ae4ab`, `broca` working tree.

Design note §10 specifies fusiform's consumer push in detail: two envelope
shapes, a monotonic ordering rule, a high-water re-sync, and — the part it
argues hardest for — **a discriminated acknowledgement**, modelled on
`broca-protocol`'s `ApprovalResponse`, so a sender can tell an applied push
from a no-op without inferring it from an echoed watermark.

Three facts about the transport make that unbuildable in its current form. All
three were read from source, not inferred.

## 1. A push carries no reply

`ModuleHandle::push` emits `FrameType::Push` with `corr: 0`
(`subc-client-rs/src/lib.rs:225`, and `assert_push` in
`subc-core/tests/forwarding.rs:4852` pins the zero). Correlation zero is what
makes a frame uncorrelated: there is no outstanding request for a reply to
settle against, so nothing can come back.

A module's entire outbound vocabulary is three methods — `catalog_update`,
`push`, and a dropped-frame counter. There is no module-initiated request
frame, so fusiform cannot ask a consumer anything and cannot receive an
answer.

**Consequence:** the discriminated acknowledgement has no transport. Not "is
not implemented yet" — there is no channel it could travel on.

## 2. Both consumer clients discard push frames

- `subc-client-rs/src/consumer.rs:3053` — `FrameType::Push => {}`. The frame is
  read, matched, and dropped. A consumer using the shared Rust client cannot
  observe a push at all.
- `broca-subc/src/connection.rs:867` and `:951` — `FrameType::Push => continue,
  // interim progress — ignore`. BROCA's own client, twice, deliberately.

The daemon *does* forward pushes to the consumer's socket
(`subc-core/tests/forwarding.rs:3546` reads one off the wire), so this is a
client-layer decision rather than a routing gap. The bytes arrive and are
thrown away.

**Consequence:** a doorbell push to either consumer today reaches their socket
and dies there. Both would need a client change before any push design of mine
is observable.

## 3. What a consumer *does* receive

`StreamData` on a held-open subscription. `SubcConsumer::subscribe` returns a
`Subscription` whose `events()` channel is fed from `route_stream_data`, which
is driven by `FrameType::StreamData`. On the module side that is
`RequestCtx::emit` — available only inside a live request, tied to that
request's `(channel, corr)`.

So the shape the transport actually supports is: **a consumer subscribes and
holds the request open; fusiform emits on it.** That is a pull-shaped
relationship wearing a push coat, and it inverts who owns the lifecycle.

## What this does not settle

Whether to change fusiform's design or ask for a transport change is not mine
alone: `subc-client-rs` is SUBC's, and BROCA's client is BROCA's. Two of the
three facts above are decisions those owners made deliberately — the comment
"interim progress — ignore" is not an oversight.

What is settled is that §10's acknowledgement paragraph describes something
that cannot exist on the current push path, and that both consumers would
need to change before any push arrives at all.

## How this went unnoticed

The design note was written before any code and cites the acknowledgement
shape from `broca-protocol/src/approval.rs`, which is real and does exactly
what the note says. The error is one level up: a *response type* exists, and
the note assumed a *response channel* existed to carry it. Those are different
claims, and the second was never checked.

The tell, in hindsight, is that the section argues about the acknowledgement's
SHAPE at length — which arm names a partial apply, when a reason earns a wire
arm — and never states how the acknowledgement gets back. A design that is
detailed about the contents of a message and silent about its direction of
travel has usually not been walked end to end.
