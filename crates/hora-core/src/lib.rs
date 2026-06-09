//! Core of Hora: configuration, probing, storage, TLS-expiry and scheduling.

pub mod cert;
pub mod coalesce;
pub mod config;
pub mod db;
pub mod http;
pub mod import;
pub mod notifications;
pub mod peer;
pub mod probe;
pub mod scheduler;
pub mod slo;
pub mod supervisor;
pub mod topology;

/// Seconds in a day (UTC), shared across the time-bucketing logic.
pub const SECONDS_PER_DAY: i64 = 86_400;
