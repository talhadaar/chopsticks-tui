//! Column diff computation (ticket T08).

use std::collections::BTreeMap;

use crate::contracts::{BlockColumn, CellDiff, PinnedItemId, RenderCtx, ValueRenderer};

/// Compute per-item diffs between two adjacent block columns, comparing on the
/// rendered strings. (Stub — implemented in T08.)
pub fn diff_columns(
    _prev: &BlockColumn,
    _next: &BlockColumn,
    _renderer: &dyn ValueRenderer,
    _ctx: &RenderCtx,
) -> BTreeMap<PinnedItemId, CellDiff> {
    todo!("T08: per-item Unchanged/Changed/Added/Removed")
}
