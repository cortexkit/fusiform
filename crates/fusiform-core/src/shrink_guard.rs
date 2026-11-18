//! The same shrink policy applies to observed catalogs and published releases.

/// The fraction of the catalog a single poll may drop before fusiform refuses.
///
/// Grounded in two measurements rather than chosen. Across two live fetches six
/// hours apart, ZERO models disappeared out of 6,253 — the real rate is not
/// merely low, it was nil over that interval. And the largest single provider
/// carries 9.9% of the catalog, so a whole provider vanishing at once, the
/// biggest plausible legitimate drop, stays under this.
///
/// A legitimate mass withdrawal beyond that is possible and would be refused.
/// That is the intended direction: it presents as a Degraded module with a
/// named reason and an operator decision, while the alternative presents as a
/// successful poll that emptied the catalog. One is a delay, the other is
/// silent data loss with every component reporting success.
pub const MAX_SHRINK_FRACTION: f64 = 0.25;

/// Below this many models, the fraction is meaningless and any drop is allowed.
///
/// A store holding four models cannot express a 25% threshold usefully, and a
/// small catalog is either a test fixture or a brand-new install — neither is a
/// state worth defending against a shrink.
pub const SHRINK_GUARD_MIN_MODELS: usize = 100;

/// Whether a document drops so much of the catalog that fusiform refuses it.
///
/// `held` is the number of models currently present in the catalog.
pub fn implausible_shrink(held: usize, incoming: usize) -> Option<String> {
    if held < SHRINK_GUARD_MIN_MODELS {
        return None;
    }

    if incoming >= held {
        // GROWTH IS DELIBERATELY UNGUARDED, and this line is where a reader
        // would otherwise have to guess whether that was reasoned or missed.
        //
        // The two directions fail differently, and the difference is the same
        // loud-versus-silent one that decides most of this crate's design:
        //
        //   - A spurious SHRINK writes tombstones. A consumer stops offering a
        //     model that really exists, nothing errors, and nobody looks. That
        //     is the failure the guard was built for, from a measured event: a
        //     collapsed response took a seeded store from 14 models to 1.
        //
        //   - A spurious GROWTH writes arrivals. A consumer routes to a model
        //     the provider does not have and gets a 404 on the first request —
        //     immediate, attributable, and self-correcting on the next poll
        //     that omits it.
        //
        // So the direction that needs a guard is the one that cannot announce
        // itself. Refusing growth would trade a loud failure for a refused
        // poll, and a real batch arrival is ordinary: 51 models landed in a
        // single poll on 2026-08-14, and models.dev adds roughly 45 a day.
        //
        // WHAT WOULD CHANGE THIS: growth is not harmless in the store, only at
        // the consumer. A duplicated provider index would append tens of
        // thousands of arrival eras that append-only history can never remove,
        // only tombstone. No such event has been observed, so building for it
        // would be designing against an imagined failure — but if one is ever
        // seen, this is the line that needs the counterpart, and the argument
        // above is what it has to beat.
        //
        // WHAT ORDINARY LOOKS LIKE, measured on the live store 2026-08-15 so a
        // future threshold is picked against data rather than taste. The
        // largest single-poll arrival to date: 211 models against ~6,370 held,
        // which is 3.3% — and 204 of them were one provider (`edenai`) growing
        // from 16 models to 220 in a day. A real provider onboarding its
        // catalogue is therefore a THREE PERCENT jump, so any growth guard has
        // to sit well above that, while the failure it would catch (an index
        // duplicated into itself) is nearer a hundred percent. That gap is
        // wide, which is a further reason the guard is not urgent: there is no
        // ambiguous middle to adjudicate.
        return None;
    }

    let dropped = held - incoming;
    let fraction = dropped as f64 / held as f64;
    if fraction <= MAX_SHRINK_FRACTION {
        return None;
    }

    Some(format!(
        "upstream document describes {incoming} models, {dropped} fewer than the \
         {held} currently held ({:.1}% drop, limit {:.0}%); refusing to tombstone \
         them on one response",
        fraction * 100.0,
        MAX_SHRINK_FRACTION * 100.0
    ))
}
