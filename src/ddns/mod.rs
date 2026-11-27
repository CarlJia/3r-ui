//! Cloudflare DDNS dynamic domain resolution subsystem.

pub mod config;
pub mod runner;

pub use runner::{perform_ddns_sync, run_ddns_loop, DdnsNotifier, DnsSyncOutcome};
