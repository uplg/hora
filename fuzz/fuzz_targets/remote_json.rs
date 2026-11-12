//! The JSON a third party answers: a registry's RDAP domain object, GitHub's
//! latest release (and the summary drawn from its notes), the version a
//! monitored service reports about itself.
#![no_main]

use std::sync::LazyLock;

use hora_core::fuzzing::{rdap, release};
use libfuzzer_sys::fuzz_target;
use serde_json_path::JsonPath;

/// `release.current_query` values, as an operator writes them.
static QUERIES: LazyLock<Vec<JsonPath>> = LazyLock::new(|| {
    ["$.server.version", "$..version", "$[0]", "$.*", "$..*"]
        .into_iter()
        .map(|query| JsonPath::parse(query).expect("a valid query"))
        .collect()
});

fuzz_target!(|data: &[u8]| {
    let text = hora_fuzz::text(data);
    for query in QUERIES.iter() {
        let _ = release::version_in(&text, Some(query));
    }
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let _ = rdap::expiration_event(&body);
    if let Some((tag, _url, notes)) = release::release_of(&body) {
        let _ = release::is_newer(&tag, hora_core::updates::RUNNING);
        let _ = release::is_newer(hora_core::updates::RUNNING, &tag);
        let summary = hora_core::updates::fuzz::summary(&notes);
        // 300 characters and an ellipsis in an alert, whatever the notes weigh.
        assert!(summary.chars().count() <= 301, "{}", summary.len());
    }
});
