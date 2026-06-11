//! Value rendering: `scale_value::Value` -> compact display string (ticket T07).

use scale_value::Value;

use crate::contracts::{PathSeg, RenderCtx, ValueRenderer};

/// Type-aware value renderer (SS58, balances, enums, structs, `Option`, bytes).
/// (Stub — implemented in T07.)
pub struct DefaultRenderer;

impl ValueRenderer for DefaultRenderer {
    fn render(&self, _value: &Value<u32>, _path: &[PathSeg], _ctx: &RenderCtx) -> String {
        todo!("T07: navigate path + type-aware formatting")
    }
}
