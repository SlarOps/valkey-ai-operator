pub mod k8s;

use crate::agent::tool::Tool;

/// Stub: returns empty tool list. Will be replaced with role-based registration in Phase 3.
pub fn register_tools() -> Vec<Box<dyn Tool>> {
    Vec::new()
}
