//! The extensions this library has built in: none now, as each has gone to rradius-enterprise.

use crate::extension::{Extension, Extensions};
use std::sync::Arc;

/// The extensions built into this library.
pub fn builtin() -> Extensions {
    let list: Vec<Arc<dyn Extension>> = Vec::new();
    // Without the checks of `Extensions::new`: a test runs them on this list.
    Extensions::unchecked(list)
}
