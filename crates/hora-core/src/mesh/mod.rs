//! The mesh: Hora instances watching and helping each other.
//!
//! - [`peer`]: mutual surveillance (outbound dead-man heartbeats, peer watches
//!   with a witness quorum, and the `/healthz` report).
//! - [`confirm`]: multi-vantage confirmation of a local down before alerting.
//! - [`fingerprint`]: what a monitor asserts, as one value two nodes compare.
//! - [`vantage`]: per-vantage latency, polled from each peer's monitor listing.
//! - [`wire`]: the JSON shapes those exchanges carry.

pub mod confirm;
pub mod fingerprint;
pub mod peer;
pub mod vantage;
pub mod wire;
