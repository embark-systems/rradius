//! Types every protocol and handler shares: attribute values and the result of an
//! authentication step. They do not depend on Lua.
use std::sync::Arc;

pub mod framing;
pub mod hex;
pub mod seal;
pub mod tls;
pub mod wpa;

/// A protocol attribute value as seen by Lua: raw bytes become a Lua string,
/// numbers a Lua number.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Bytes(Vec<u8>),
    Number(f64),
}

/// Checks a `set_attr(name, value)` call; an `Err` is raised as a Lua error.
pub type AttrValidator = Arc<dyn Fn(&str, &AttrValue) -> Result<(), String> + Send + Sync>;

/// Result of one authentication step: a script or an auth chain handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Accept,
    Reject,
    /// Leave the decision to the next handler; only meaningful in an auth chain.
    Ignore,
    /// Ask for more with an Access-Challenge; only for RADIUS authentication.
    Challenge,
}
