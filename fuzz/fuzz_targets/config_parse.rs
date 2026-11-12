//! The configuration file, as the operator writes it: TOML, `${VAR}`
//! expansion, deserialization and validation, then what first reads a loaded
//! configuration.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // What every `${VAR}` holds: unset, empty, a plain value, or one that
    // would break TOML if it were pasted into the text.
    let value = match data.len() % 4 {
        0 => None,
        1 => Some(""),
        2 => Some("value"),
        _ => Some("a\"b\n[c] ${D} { } \u{e9}"),
    };
    let toml = hora_fuzz::text(data);
    let Ok(config) = hora_core::config::fuzz::parse_with(&toml, value) else {
        return;
    };
    let _ = format!("{config:?}");
    for monitor in &config.monitors {
        let _ = hora_core::mesh::fingerprint::check_fingerprint(monitor);
        let _ = hora_core::cert::monitor_endpoint(monitor);
        let _ = hora_core::mesh::vantage::key(monitor.kind(), monitor.target());
        let _ = hora_core::topology::transitive_upstreams(&config.monitors, &monitor.id);
        let _ = hora_core::topology::transitive_dependents(&config.monitors, &monitor.id);
        let _ = config.fail_threshold(&monitor.id);
    }
    for peer in &config.peers {
        let _ = peer.effective_witness_url();
    }
    let schedule = hora_core::planned::Schedule::merged(&config, &[]);
    for window in schedule.windows() {
        let _ = window.covered_peers(&config);
    }
});
