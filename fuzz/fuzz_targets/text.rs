//! The small parsers and escapers a request, a peer or a monitored server
//! feeds with text: what is printed (`printable`, `bounded`), what goes into
//! a URL or XML, a stored selector, `expected_status`, a target's secrets, a
//! plugin's output, a proxy URL's credentials, release notes and versions.
#![no_main]

use hora_core::config::ExpectedStatus;
use hora_core::planned::Selector;
use hora_core::{fmt, fuzzing};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = hora_fuzz::text(data);
    let text = text.as_ref();

    let printable = fmt::printable(text);
    assert!(
        !printable
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "printable() let a control character through"
    );
    let max = data.len() % 600;
    assert!(hora_core::bounded(text, max).chars().count() <= max);
    assert!(fmt::percent_encode(text).is_ascii());
    let escaped = fmt::xml_escape(text);
    assert!(!escaped.contains(['<', '>', '"', '\'']));

    // A stored selector reads back as it was written.
    if let Some(selector) = Selector::parse(text) {
        assert_eq!(Selector::parse(&selector.to_string()), Some(selector));
    }
    let _ = hora_core::planned::announcing_peer(text);
    // The canonical form of `expected_status` is what peers compare.
    if let Ok(statuses) = ExpectedStatus::parse_list(text) {
        assert_eq!(ExpectedStatus::parse_list(&statuses.to_string()), Ok(statuses));
    }

    // Peers compare targets masked on both sides: masking twice must change
    // nothing more.
    let disclosed = hora_core::mesh::wire::disclosed_target(text);
    assert_eq!(
        hora_core::mesh::wire::disclosed_target(&disclosed),
        disclosed,
        "masking a target is not idempotent"
    );
    let _ = hora_core::config::split_host_port(text);
    let _ = hora_core::cert::fuzz::host_port(text);

    let _ = fuzzing::exec::first_line(data);
    let _ = fuzzing::tunnel::percent_decode(text);
    let _ = fuzzing::tunnel::base64(data);
    let _ = hora_core::updates::fuzz::summary(text);
    let (latest, current) = text.split_once('\n').unwrap_or((text, ""));
    let _ = fuzzing::release::is_newer(latest, current);
    let _ = fuzzing::release::version_in(text, None);

    let _ = hora_notify::AlertSeverity::parse(text);
    let _ = hora_core::status::CheckStatus::parse(text);
});
