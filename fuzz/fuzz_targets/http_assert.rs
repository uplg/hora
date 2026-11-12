//! What an HTTP probe does with a monitored server's answer: the body
//! assertions (keyword, JSON query, number), and the failure snapshot kept
//! with an incident (headers and the start of the body).
//!
//! Input: four lines - `keyword`, `json_query`, `json_expected`,
//! `number_regex` (an empty line: not set) - then the answer's body. The
//! assertions are the operator's, the body is the server's.
#![no_main]

use hora_core::config::MonitorKind;
use hora_core::probe::fuzz;
use libfuzzer_sys::fuzz_target;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// A TOML `key = "value"` line, or nothing for an empty value.
fn setting(key: &str, value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!("{key} = {}\n", toml::Value::String(value.to_owned()))
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = hora_fuzz::Input::new(data);
    let (keyword, query, expected, regex) =
        (input.line(), input.line(), input.line(), input.line());
    let body = input.rest();

    let config = format!(
        "[page]\n[server]\n[[monitors]]\nid = \"m\"\nname = \"M\"\n\
         target = \"http://127.0.0.1:9/\"\ninterval_secs = 60\n{}{}{}{}{}",
        setting("keyword", &keyword),
        setting("json_query", &query),
        // Only read with a query.
        if query.is_empty() {
            String::new()
        } else {
            setting("json_expected", &expected)
        },
        setting("number_regex", &regex),
        if regex.is_empty() {
            ""
        } else {
            "number_min = -5\nnumber_max = 1000\n"
        },
    );
    if let Ok(config) = hora_core::config::parse(&config)
        && let Some(MonitorKind::Http(spec)) = config.monitors.first().map(|monitor| &monitor.spec)
    {
        let _ = fuzz::check_assertions(spec, body);
    }

    // The answer's headers: one per line of the body, as far as HTTP lets a
    // server send them.
    let mut headers = HeaderMap::new();
    for line in body.split(|byte| *byte == b'\n').take(40) {
        let (name, value) = line.split_at(line.len().min(12));
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name),
            HeaderValue::from_bytes(value),
        ) {
            headers.append(name, value);
        }
    }
    let (snapshot, snippet) = fuzz::snapshot(&headers, body);
    // ~6 KiB at worst, as the module says: 24 headers of 160 characters and
    // 2048 of body.
    assert!(snapshot.chars().count() <= 64 + 25 * 161 + 2 + 2048);
    assert!(snippet.chars().count() <= 300);
});
