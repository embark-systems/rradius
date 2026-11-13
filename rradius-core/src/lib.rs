#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod accounts;
pub mod admin;
pub mod aligned_alloc;
pub mod auth_chain;
pub mod cli;
pub mod client_source;
pub mod clients;
pub mod common;
pub mod config;
pub mod dictionary;
pub mod dynamic_authorization;
pub mod eap;
pub mod eap_method;
pub mod extension;
pub mod extensions;
pub mod forwarding;
pub mod ldap_handler;
pub mod logging;
pub mod metrics;
pub mod radius;
pub mod records;
pub mod reload;
pub mod revocation;
pub mod runtime;
pub mod schema;
pub mod second_factor;
pub mod script_http;
pub mod session_resumption;
pub mod spool;
pub mod mschap;
pub mod pre_chain;
pub mod tacacs;
pub mod tacacs_rules;
pub mod tacacs_transport;
pub mod totp;
pub mod tuning;
pub mod config_tools;
#[cfg(feature = "backup")]
pub mod backup;
pub mod lua_engine;
pub mod passwords;
#[cfg(test)]
mod scenario_scripts;
// Test code, where an unwrap that panics is a failed test.
#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub mod test_support;
