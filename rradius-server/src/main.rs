#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use rradius_core::cli::{self, Program};

#[global_allocator]
static ALLOCATOR: rradius_core::aligned_alloc::CacheLineAligned = rradius_core::aligned_alloc::CacheLineAligned;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    cli::main(Program { name: "rradius-server", version: env!("CARGO_PKG_VERSION"), extensions: rradius_core::extensions::builtin() })
}
