//! Compatibility wrapper delegating built-in model family caps to [`crate::model_registry`].
use super::catalog::ModelCap;

/// Built-in 2026-11 model family capability table. Longest fragment wins.
pub fn builtin_model_cap(model_id: &str) -> Option<ModelCap> {
    crate::model_registry::builtin_model_cap(model_id)
}
