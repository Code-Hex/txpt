pub mod cli;
pub mod diff;
pub mod ignore;
pub mod json;
pub mod manifest;
pub mod platform;
pub mod rollback;
pub mod root;
pub mod runner;
pub mod snapshot;
pub mod storage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
