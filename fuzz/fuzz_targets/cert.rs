//! The leaf certificate a monitored server presents (DER). The watcher does
//! not verify trust, so these bytes are the server's to choose: its expiry
//! and public-key fingerprint are read from them.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some((not_after, fingerprint)) = hora_core::cert::fuzz::leaf_facts(data) {
        assert_eq!(fingerprint.len(), 64);
        // What the watcher then computes and words.
        let now = 1_800_000_000_i64;
        let secs_left = not_after - now;
        let _ = secs_left / hora_core::SECONDS_PER_DAY;
        let _ = hora_core::fmt::utc(not_after);
        let _ = hora_notify::fuzz::render_all(hora_notify::Event::CertExpiring {
            monitor: "m",
            secs_left,
        });
    }
});
