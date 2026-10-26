//! `review` as a library, so the integration tests can drive the provider
//! runners against stub providers with the real binary as their keeper (see
//! `keeper`). Cargo hands an integration test the built binary's path
//! (`CARGO_BIN_EXE_review`) and gives a unit test nothing of the kind, which is
//! the whole reason for the split; nothing here is a public API.

pub mod app;
pub mod audit;
pub mod cli;
pub mod config;
pub mod config_cmd;
pub mod config_write;
pub mod contained;
pub mod grok_home;
pub mod incident;
pub mod inflight;
pub mod input;
pub mod interrupt_cmd;
pub mod keeper;
pub mod lock;
pub mod man;
pub mod project;
pub mod prompt;
pub mod provider;
pub mod sessions;
pub mod timings;
pub mod transcript;
pub mod watchdog;
pub mod writable_roots;
