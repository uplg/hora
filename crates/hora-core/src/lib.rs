//! Core of Hora: configuration, probing, storage, TLS-expiry and scheduling.

pub mod cert;
pub mod config;
pub mod db;
pub mod http;
pub mod notifications;
pub mod probe;
pub mod scheduler;
pub mod supervisor;

/// Seconds in a day (UTC), shared across the time-bucketing logic.
pub const SECONDS_PER_DAY: i64 = 86_400;
