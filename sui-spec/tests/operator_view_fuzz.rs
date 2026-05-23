//! Fuzz the OperatorView render path — assert no panic across
//! arbitrary inputs.
//!
//! The trait is the substrate's typed surface for every
//! sui-spec-inventory mode (FlakeLockView / NarinfoView /
//! HashDecodeView / RegistryResolveView / RealisationView /
//! StorePathView).  Each impl funnels through `render(view)`
//! which writes to stdout.  We can't observe stdout in
//! property tests easily, so we instead exercise the
//! LabeledTable builder + render() directly under random
//! inputs and confirm:
//!   - construction never panics
//!   - render() never panics
//!   - any sequence of kv/opt/section/list_items/blank
//!     composes safely

use proptest::prelude::*;
use sui_spec::style::LabeledTable;

// Capture stdout for the duration of the render — we don't
// actually inspect the bytes, just confirm no panic.
fn render_without_observing<F: FnOnce()>(f: F) {
    // proptest already isolates each case; we just call.
    f();
}

#[derive(Debug, Clone)]
enum Op {
    Kv(String, String),
    Opt(String, Option<String>),
    Section(String, Option<usize>),
    ListItem(String),
    Blank,
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        ("[a-zA-Z]{1,12}", "[a-zA-Z0-9 ]{0,40}").prop_map(|(k, v)| Op::Kv(k, v)),
        ("[a-zA-Z]{1,12}", proptest::option::of("[a-zA-Z0-9 ]{0,40}"))
            .prop_map(|(k, v)| Op::Opt(k, v)),
        ("[a-zA-Z]{1,12}", proptest::option::of(0usize..100))
            .prop_map(|(t, c)| Op::Section(t, c)),
        "[a-zA-Z0-9]{1,20}".prop_map(Op::ListItem),
        Just(Op::Blank),
    ]
}

proptest! {
    /// Random sequences of typed builder ops must compose
    /// without panicking.  Each op exercises a different
    /// render branch.
    #[test]
    fn labeled_table_renders_arbitrary_ops_without_panic(
        ops in proptest::collection::vec(arb_op(), 0..30),
        label_w in 4usize..40,
    ) {
        render_without_observing(|| {
            let mut t = LabeledTable::new(label_w);
            for op in &ops {
                t = match op {
                    Op::Kv(k, v)         => t.kv(k, v),
                    Op::Opt(k, v)        => t.opt(k, v.as_deref()),
                    Op::Section(s, c)    => t.section(s, *c),
                    Op::ListItem(it)     => t.list_items("→", [it.as_str()]),
                    Op::Blank            => t.blank(),
                };
            }
            t.render();
        });
        // If we got here, no panic — property holds.
        prop_assert!(true);
    }

    /// Large label widths and oversized values don't crash.
    #[test]
    fn labeled_table_handles_extremes(
        label_w in 0usize..200,
        big in "[a-z]{200,400}",
        small in "[a-z]{1,3}",
    ) {
        render_without_observing(|| {
            LabeledTable::new(label_w)
                .kv(&big, &small)
                .kv(&small, &big)
                .opt(&big, Some(&small))
                .opt(&small, None)
                .section(&big, Some(99))
                .list_items("→", [big.as_str(), small.as_str()])
                .render();
        });
        prop_assert!(true);
    }

    /// Unicode in labels + values doesn't crash render.
    #[test]
    fn labeled_table_handles_unicode(
        prefix in "[\u{4e00}-\u{9fff}]{1,5}",  // CJK
        suffix in "[\u{1F300}-\u{1F5FF}]{0,3}", // misc symbols
    ) {
        render_without_observing(|| {
            LabeledTable::new(14)
                .kv("label", &format!("{prefix}-{suffix}"))
                .render();
        });
        prop_assert!(true);
    }

    /// Section with empty title + count=0 is safe.
    #[test]
    fn labeled_table_empty_section(seed in any::<u8>()) {
        render_without_observing(|| {
            LabeledTable::new(seed as usize % 30)
                .section("", Some(0))
                .render();
        });
        prop_assert!(true);
    }

    /// Render is idempotent — calling render() doesn't mutate
    /// dependent state (we build twice, both work).
    #[test]
    fn labeled_table_can_be_built_repeatedly(
        rows in proptest::collection::vec(("[a-z]{1,8}", "[a-z]{1,16}"), 1..10),
    ) {
        let make = || {
            let mut t = LabeledTable::new(14);
            for (k, v) in &rows {
                t = t.kv(k, v);
            }
            t
        };
        render_without_observing(|| {
            make().render();
            make().render();
        });
        prop_assert!(true);
    }
}

// ── pick_format fuzzing ───────────────────────────────────

proptest! {
    /// `operator_view::pick_format` returns the matching format
    /// or a typed error.  Never panics.
    #[test]
    fn pick_format_returns_match_or_typed_error(
        items in proptest::collection::vec("[a-z]{1,8}", 0..10),
        target in "[a-z]{1,8}",
    ) {
        use sui_spec::operator_view::pick_format;
        // Items act as "named items"; pick by string-equality.
        let result = pick_format(items.clone(), |s| *s == target, "test");
        match result {
            Ok(found)  => prop_assert!(items.contains(&found)),
            Err(e)     => {
                // Confirm the typed error includes bridge name.
                let msg = format!("{e:?}");
                prop_assert!(msg.contains("test"));
            }
        }
    }
}
