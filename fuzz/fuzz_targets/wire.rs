//! What a peer sends, as JSON: the `/healthz` report a witness reads, a
//! heartbeat's body, a confirmation probe's request, the monitors a peer
//! discloses - each read from the same bytes, and passed to what reads it
//! next without a database.
#![no_main]

use hora_core::mesh::wire::{
    HealthReport, HeartbeatBody, PeerMonitors, ProbeRequest, ProbeResponse, disclosed_target,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(report) = serde_json::from_slice::<HealthReport>(data) {
        let _ = report.status.as_str();
        for seen in report.peers.values() {
            let _ = (seen.state.as_str(), hora_core::fmt::duration(seen.age));
        }
        let _ = serde_json::to_vec(&report);
    }
    if let Ok(body) = serde_json::from_slice::<HeartbeatBody>(data) {
        for window in &body.maintenance {
            let _ = hora_core::fmt::utc(window.starts_at);
            let _ = hora_core::bounded(&window.reason, hora_core::MAX_SILENCE_REASON_CHARS);
        }
        let _ = serde_json::to_vec(&body);
    }
    if let Ok(request) = serde_json::from_slice::<ProbeRequest>(data) {
        // The responder masks the target before comparing it with its own.
        let _ = disclosed_target(&request.target);
        let _ = hora_core::mesh::vantage::key(request.kind, &request.target);
        let _ = hora_core::config::Monitor::ad_hoc(request.kind, request.target.clone());
        let _ = serde_json::to_vec(&request);
    }
    if let Ok(monitors) = serde_json::from_slice::<PeerMonitors>(data) {
        for monitor in &monitors.monitors {
            let _ = hora_core::mesh::vantage::key(monitor.kind, &monitor.target);
            // How a vantage's median is shown on the status page.
            let _ = monitor.p50_ms.map(hora_core::fmt::latency);
        }
        let _ = serde_json::to_vec(&monitors);
    }
    let _ = serde_json::from_slice::<ProbeResponse>(data);
});
