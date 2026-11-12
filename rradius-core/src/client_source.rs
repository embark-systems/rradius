//! Clients the configuration does not list, found by their UDP source address: what a program's
//! extension can offer the RADIUS server, such as clients looked up in the database.

use crate::clients::ClientTable;
use crate::config::ServerConfig;
use crate::lua_engine::Db;
use crate::metrics::Metrics;
use futures::future::BoxFuture;
use std::net::IpAddr;
use std::sync::Arc;

/// What is known of a source a client source was asked about.
pub enum Answer {
    Found(Arc<ClientTable>),
    /// Not a client of this source.
    NotFound,
    /// Not now: being looked up, too busy, or the lookup failed.
    Unknown,
}

/// Clients the configuration does not list, found by their UDP source address. Asked only about
/// sources no configured client covers.
pub trait ClientSource: Send + Sync {
    /// For a packet: the table to identify `source` in, `None` to discard it as unknown.
    fn table_for<'a>(&'a self, source: IpAddr) -> BoxFuture<'a, Option<Arc<ClientTable>>>;

    /// For a spooled request being replayed: what is known of `source` now.
    fn answer_for<'a>(&'a self, source: IpAddr) -> BoxFuture<'a, Answer>;
}

/// What a client source is made from, at the start and at each reload, which starts a new one.
pub struct ClientSourceContext<'a> {
    pub config: &'a ServerConfig,
    pub db: &'a Db,
    pub metrics: &'a Arc<Metrics>,
}
