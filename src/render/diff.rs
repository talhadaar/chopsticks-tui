//! Column diff computation (ticket T08).

use std::collections::{BTreeMap, BTreeSet};

use crate::contracts::{BlockColumn, CellDiff, CellState, PinnedItemId, RenderCtx, ValueRenderer};

/// Sentinel comparison string for a `Missing` cell. Chosen so it can never
/// collide with a rendered value (the renderer produces value text, never this
/// lone glyph by construction in practice).
const MISSING_SENTINEL: &str = "\u{2205}"; // ∅

/// Render one [`CellState`] to the string used for diff comparison.
///
/// Comparison rules (a `Missing` cell is treated as a distinct comparison
/// string, so):
/// - `Missing` vs a value → strings differ → `Changed`.
/// - `Missing` vs `Missing` → identical sentinels → `Unchanged`.
/// - `Undecodable` carries its error into the sentinel, so two undecodable
///   cells with the same error compare equal, and an undecodable vs a value
///   differs.
fn compare_string(state: &CellState, renderer: &dyn ValueRenderer, ctx: &RenderCtx) -> String {
    match state {
        // Whole value, empty path.
        CellState::Value(v) => renderer.render(v, &[], ctx),
        CellState::Missing => MISSING_SENTINEL.to_string(),
        CellState::Undecodable { error, .. } => format!("\u{2205}undecodable:{error}"),
    }
}

/// Compute per-item diffs between two adjacent block columns, comparing on the
/// rendered strings.
///
/// For each [`PinnedItemId`] in the union of `prev.cells` and `next.cells`:
/// - present only in `next` → [`CellDiff::Added`];
/// - present only in `prev` → [`CellDiff::Removed`];
/// - present in both → render each side to a comparison string and compare:
///   equal → [`CellDiff::Unchanged`], else
///   [`CellDiff::Changed`]`{ from, to }`.
pub fn diff_columns(
    prev: &BlockColumn,
    next: &BlockColumn,
    renderer: &dyn ValueRenderer,
    ctx: &RenderCtx,
) -> BTreeMap<PinnedItemId, CellDiff> {
    let ids: BTreeSet<PinnedItemId> = prev.cells.keys().chain(next.cells.keys()).copied().collect();

    ids.into_iter()
        .map(|id| {
            let diff = match (prev.cells.get(&id), next.cells.get(&id)) {
                (None, None) => unreachable!("id came from the union of both maps"),
                (None, Some(_)) => CellDiff::Added,
                (Some(_), None) => CellDiff::Removed,
                (Some(p), Some(n)) => {
                    let from = compare_string(p, renderer, ctx);
                    let to = compare_string(n, renderer, ctx);
                    if from == to {
                        CellDiff::Unchanged
                    } else {
                        CellDiff::Changed { from, to }
                    }
                }
            };
            (id, diff)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{BlockRef, PathSeg};
    use scale_value::Value;
    use subxt::utils::H256;

    /// Deterministic renderer: emits a fixed string per value so the diff logic
    /// is the only thing under test. We key off the value's debug form.
    struct MockRenderer;

    impl ValueRenderer for MockRenderer {
        fn render(&self, value: &Value<u32>, _path: &[PathSeg], _ctx: &RenderCtx) -> String {
            format!("{value:?}")
        }
    }

    fn ctx() -> RenderCtx {
        RenderCtx {
            ss58_prefix: 42,
            token_decimals: 12,
            token_symbol: "UNIT".to_string(),
        }
    }

    fn block(number: u32) -> BlockRef {
        BlockRef {
            number,
            hash: H256::zero(),
        }
    }

    fn column(number: u32, cells: Vec<(u64, CellState)>) -> BlockColumn {
        BlockColumn {
            block: block(number),
            cells: cells
                .into_iter()
                .map(|(id, state)| (PinnedItemId(id), state))
                .collect(),
        }
    }

    /// A `Value<u32>` carrying a dummy type-id context, as the contract requires.
    fn value_u32(n: u128) -> Value<u32> {
        Value::u128(n).map_context(|_| 0u32)
    }

    fn val(n: u128) -> CellState {
        CellState::Value(value_u32(n))
    }

    #[test]
    fn unchanged_when_same_rendered_value() {
        let prev = column(1, vec![(0, val(7))]);
        let next = column(2, vec![(0, val(7))]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        assert_eq!(out[&PinnedItemId(0)], CellDiff::Unchanged);
    }

    #[test]
    fn changed_carries_from_and_to() {
        let prev = column(1, vec![(0, val(7))]);
        let next = column(2, vec![(0, val(8))]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        match &out[&PinnedItemId(0)] {
            CellDiff::Changed { from, to } => {
                assert_eq!(from, &MockRenderer.render(&value_u32(7), &[], &ctx()));
                assert_eq!(to, &MockRenderer.render(&value_u32(8), &[], &ctx()));
                assert_ne!(from, to);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn added_when_only_in_next() {
        let prev = column(1, vec![]);
        let next = column(2, vec![(0, val(7))]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        assert_eq!(out[&PinnedItemId(0)], CellDiff::Added);
    }

    #[test]
    fn removed_when_only_in_prev() {
        let prev = column(1, vec![(0, val(7))]);
        let next = column(2, vec![]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        assert_eq!(out[&PinnedItemId(0)], CellDiff::Removed);
    }

    #[test]
    fn missing_vs_value_is_changed() {
        let prev = column(1, vec![(0, CellState::Missing)]);
        let next = column(2, vec![(0, val(7))]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        match &out[&PinnedItemId(0)] {
            CellDiff::Changed { from, to } => {
                assert_eq!(from, MISSING_SENTINEL);
                assert_ne!(from, to);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn missing_vs_missing_is_unchanged() {
        let prev = column(1, vec![(0, CellState::Missing)]);
        let next = column(2, vec![(0, CellState::Missing)]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        assert_eq!(out[&PinnedItemId(0)], CellDiff::Unchanged);
    }

    #[test]
    fn undecodable_vs_value_is_changed() {
        let prev = column(
            1,
            vec![(
                0,
                CellState::Undecodable {
                    raw_hex: "0xdead".to_string(),
                    error: "bad type".to_string(),
                },
            )],
        );
        let next = column(2, vec![(0, val(7))]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        match &out[&PinnedItemId(0)] {
            CellDiff::Changed { from, to } => {
                assert!(from.contains("bad type"), "error sentinel should carry error");
                assert_ne!(from, to);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn undecodable_same_error_is_unchanged() {
        let undec = || CellState::Undecodable {
            raw_hex: "0xbeef".to_string(),
            error: "same error".to_string(),
        };
        let prev = column(1, vec![(0, undec())]);
        let next = column(2, vec![(0, undec())]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        assert_eq!(out[&PinnedItemId(0)], CellDiff::Unchanged);
    }

    #[test]
    fn diffs_union_of_ids() {
        // id 0 in both (changed), id 1 only prev (removed), id 2 only next (added).
        let prev = column(1, vec![(0, val(1)), (1, val(9))]);
        let next = column(2, vec![(0, val(2)), (2, val(5))]);
        let out = diff_columns(&prev, &next, &MockRenderer, &ctx());
        assert_eq!(out.len(), 3);
        assert!(matches!(out[&PinnedItemId(0)], CellDiff::Changed { .. }));
        assert_eq!(out[&PinnedItemId(1)], CellDiff::Removed);
        assert_eq!(out[&PinnedItemId(2)], CellDiff::Added);
    }
}
