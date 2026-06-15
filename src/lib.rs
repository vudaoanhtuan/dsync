//! `dsync` library surface. The binary (`main.rs`) is a thin entry point over these modules;
//! integration tests (`tests/`) drive the sync engine directly through this crate.

pub mod cli;
pub mod config;
pub mod delta;
pub mod error;
pub mod ignore;
pub mod progress;
pub mod server;
pub mod sync;
pub mod transport;
