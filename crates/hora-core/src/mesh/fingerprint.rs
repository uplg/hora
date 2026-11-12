//! The check fingerprint: what a monitor asserts, as one comparable value.
//!
//! Two nodes can watch the same URL and not check the same thing: another
//! expected status, a body assertion on one side only, a `Host` header that
//! selects another virtual host. A peer asked to confirm a down by kind and
//! target alone would answer with whatever monitor it has on that target, and
//! its "up" (or "down") would say nothing about the check that failed. The
//! fingerprint travels with the probe request and in the monitor listing, so
//! a peer answers only for a monitor that checks the same way, and
//! `hora peers diff` can tell "same target, checked differently".
//!
//! It is the SHA-256 of a canonical text written here field by field - never
//! of a `{:?}` rendering, which is no stable format - so two nodes on
//! different builds agree on it. Changing what goes into the text makes
//! every pair of nodes on either side of the change disagree until both are
//! upgraded: the first line carries a version for that day.

use std::fmt::Write as _;

use crate::cert::sha256_hex;
use crate::config::{
    DnsSpec, HttpMethod, HttpSpec, IcmpSpec, Monitor, NetworkProbe, Starttls, TcpSpec,
};

use super::wire::disclosed_target;

/// The fingerprint of `monitor`'s check; `None` for a push or exec monitor,
/// which no peer can repeat.
#[must_use]
pub fn check_fingerprint(monitor: &Monitor) -> Option<String> {
    canonical(monitor).map(|text| sha256_hex(text.as_bytes()))
}

/// The canonical text behind [`check_fingerprint`]: one `name=length:value`
/// line per setting that changes the verdict, in a fixed order. The length (in
/// bytes) keeps a value holding a line break from passing for two fields.
///
/// In: the kind, the target as peers see it (secrets masked - a peer never
/// learns them, so it could not compare them), and per kind the assertions
/// and what selects the endpoint. Out: everything that is this node's own
/// business - timeouts, intervals, retries, thresholds, the proxy (the route,
/// not the check), the EHLO name, alert routing.
fn canonical(monitor: &Monitor) -> Option<String> {
    let probe = monitor.spec.network()?;
    let mut text = String::from("hora-check-v1\n");
    field(&mut text, "kind", monitor.kind().as_str());
    field(&mut text, "target", &disclosed_target(monitor.target()));
    match probe {
        NetworkProbe::Http(spec) => http(&mut text, spec),
        NetworkProbe::Tcp(spec) => tcp(&mut text, spec),
        NetworkProbe::Icmp(spec) => icmp(&mut text, spec),
        NetworkProbe::Dns(spec) => dns(&mut text, spec),
    }
    Some(text)
}

fn field(text: &mut String, name: &str, value: &str) {
    let _ = writeln!(text, "{name}={}:{value}", value.len());
}

fn flag(text: &mut String, name: &str, value: bool) {
    field(text, name, if value { "true" } else { "false" });
}

fn http(text: &mut String, spec: &HttpSpec) {
    // Unset means "any 2xx". A set goes in its canonical form, which for a
    // single status is the number alone and for the class `"2xx"` is what
    // unset writes: both as before lists existed, and the same check either
    // way.
    let status = spec
        .expected_status
        .as_ref()
        .map_or_else(|| "2xx".to_owned(), ToString::to_string);
    field(text, "expected_status", &status);
    // Written only when it is not the default: a monitor that says nothing
    // (every monitor of a node that predates the setting) keeps the text, and
    // so the fingerprint, it always had. A `HEAD` then never matches a peer's
    // `GET` on the same target - an old peer's included.
    if spec.method != HttpMethod::Get {
        field(text, "method", spec.method.as_str());
    }
    // Header names are case-insensitive and the map has no order: lowercase,
    // sorted. A value may be a credential: only its digest goes in, so the
    // text can be logged or shown in a test failure.
    let mut headers: Vec<(String, String)> = spec
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                sha256_hex(value.as_ref().as_bytes()),
            )
        })
        .collect();
    headers.sort();
    for (name, digest) in headers {
        field(text, "header", &name);
        field(text, "header_sha256", &digest);
    }
    if let Some(keyword) = &spec.keyword {
        field(text, "keyword", &keyword.text);
        flag(text, "keyword_invert", keyword.invert);
    }
    if let Some(json) = &spec.json {
        field(text, "json_query", json.query.raw());
        if let Some(expected) = &json.expected {
            field(text, "json_expected", expected);
        }
    }
    if let Some(number) = &spec.number {
        field(text, "number_regex", number.regex.raw());
        if let Some(min) = number.min {
            field(text, "number_min", &min.to_string());
        }
        if let Some(max) = number.max {
            field(text, "number_max", &max.to_string());
        }
    }
    // How much of the body the assertions see - which only matters to a
    // monitor that has one.
    if spec.asserts_body() {
        field(
            text,
            "body_cap_bytes",
            &spec.assertion_body_cap().to_string(),
        );
    }
    flag(text, "dual_stack", spec.dual_stack);
}

fn tcp(text: &mut String, spec: &TcpSpec) {
    match &spec.starttls {
        None => {}
        Some(Starttls::Smtp { .. }) => field(text, "starttls", "smtp"),
        Some(Starttls::Imap) => field(text, "starttls", "imap"),
    }
    flag(text, "dual_stack", spec.dual_stack);
}

fn icmp(text: &mut String, spec: &IcmpSpec) {
    flag(text, "dual_stack", spec.dual_stack);
}

fn dns(text: &mut String, spec: &DnsSpec) {
    field(text, "record", spec.record.as_str());
    if let Some(expected) = &spec.expected {
        // As the probe compares it: a set, whatever the order it is written in.
        let mut wanted: Vec<&str> = expected.split(',').map(str::trim).collect();
        wanted.sort_unstable();
        field(text, "expected", &wanted.join(","));
    }
    if let Some(resolver) = spec.resolver {
        field(text, "resolver", &resolver.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one `[[monitors]]` table of a config whose settings are `body`.
    fn monitor(body: &str) -> Monitor {
        let toml = format!(
            "[page]\n[server]\n[[monitors]]\nid = \"m\"\nname = \"M\"\ninterval_secs = 60\n{body}\n"
        );
        let config = crate::config::parse(&toml).expect("config");
        config.monitors[0].clone()
    }

    fn fingerprint(body: &str) -> String {
        check_fingerprint(&monitor(body)).expect("a network probe")
    }

    const API: &str = r#"
        target = "https://api.example.com/health"
        expected_status = 200
        keyword = "ok"
        headers = { Host = "internal.example.com", Authorization = "Bearer s3cret" }
    "#;

    /// The format is a contract between nodes on different builds: this text
    /// (and so its digest) must not change without bumping the version line,
    /// or a mesh in the middle of an upgrade stops confirming anything.
    #[test]
    fn the_canonical_text_is_pinned() {
        let host = sha256_hex(b"internal.example.com");
        let authorization = sha256_hex(b"Bearer s3cret");
        assert_eq!(
            canonical(&monitor(API)).unwrap(),
            format!(
                "hora-check-v1\n\
                 kind=4:http\n\
                 target=30:https://api.example.com/health\n\
                 expected_status=3:200\n\
                 header=13:authorization\n\
                 header_sha256=64:{authorization}\n\
                 header=4:host\n\
                 header_sha256=64:{host}\n\
                 keyword=2:ok\n\
                 keyword_invert=5:false\n\
                 body_cap_bytes=7:1048576\n\
                 dual_stack=5:false\n"
            )
        );
        assert_eq!(
            canonical(&monitor(
                "kind = \"tcp\"\ntarget = \"mail.example.com:587\"\nstarttls = \"smtp\""
            ))
            .unwrap(),
            "hora-check-v1\nkind=3:tcp\ntarget=20:mail.example.com:587\nstarttls=4:smtp\n\
             dual_stack=5:false\n"
        );
        assert_eq!(
            canonical(&monitor(
                "kind = \"dns\"\ntarget = \"example.com\"\ndns_expected = \"2.2.2.2, 1.1.1.1\""
            ))
            .unwrap(),
            "hora-check-v1\nkind=3:dns\ntarget=11:example.com\nrecord=1:A\n\
             expected=15:1.1.1.1,2.2.2.2\n"
        );
        assert_eq!(
            canonical(&monitor("kind = \"icmp\"\ntarget = \"192.0.2.1\"")).unwrap(),
            "hora-check-v1\nkind=4:icmp\ntarget=9:192.0.2.1\ndual_stack=5:false\n"
        );
        // A digest, lowercase hex: nothing of the text shows through.
        let digest = fingerprint(API);
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()), "{digest}");
        assert!(!canonical(&monitor(API)).unwrap().contains("s3cret"));
    }

    /// A monitor that sets neither `method` nor a list of statuses must
    /// keep the fingerprint it had before those settings existed: a node
    /// upgraded first would otherwise stop confirming with every peer that
    /// was not. The digests are those of 0.11.5, computed outside this code
    /// (`printf ... | shasum -a 256`).
    #[test]
    fn a_monitor_without_the_new_settings_keeps_its_fingerprint() {
        const PLAIN: &str = "target = \"https://example.com/\"";
        assert_eq!(
            canonical(&monitor(PLAIN)).unwrap(),
            "hora-check-v1\nkind=4:http\ntarget=20:https://example.com/\n\
             expected_status=3:2xx\ndual_stack=5:false\n"
        );
        assert_eq!(
            fingerprint(PLAIN),
            "933e4170b8b96229f367300730bc19c8396e63ec23db1efc7a28458e1ae74a28"
        );
        let exact = format!("{PLAIN}\nexpected_status = 204");
        assert_eq!(
            canonical(&monitor(&exact)).unwrap(),
            "hora-check-v1\nkind=4:http\ntarget=20:https://example.com/\n\
             expected_status=3:204\ndual_stack=5:false\n"
        );
        assert_eq!(
            fingerprint(&exact),
            "26b2c4d6418f5a9c539b2eadd36c5a04d52e546fc5a465a5be381a0beea14b69"
        );
        // Saying the defaults out loud is saying nothing.
        for same in [
            "method = \"GET\"",
            "method = \"get\"",
            "expected_status = \"2xx\"",
            "expected_status = [\"2xx\"]",
            "expected_status = [\"2xx\", 200]",
        ] {
            assert_eq!(
                fingerprint(&format!("{PLAIN}\n{same}")),
                fingerprint(PLAIN),
                "{same}"
            );
        }
        assert_eq!(
            fingerprint(&format!("{PLAIN}\nexpected_status = [204]")),
            fingerprint(&exact)
        );
    }

    #[test]
    fn the_method_and_the_accepted_statuses_are_part_of_the_check() {
        const PLAIN: &str = "target = \"https://example.com/\"";
        let with = |setting: &str| format!("{PLAIN}\n{setting}");
        assert_eq!(
            canonical(&monitor(&with(
                "method = \"HEAD\"\nexpected_status = [401, \"2xx\", 401]"
            )))
            .unwrap(),
            "hora-check-v1\nkind=4:http\ntarget=20:https://example.com/\n\
             expected_status=7:2xx,401\nmethod=4:HEAD\ndual_stack=5:false\n"
        );
        let all = [
            fingerprint(PLAIN),
            fingerprint(&with("method = \"HEAD\"")),
            fingerprint(&with("expected_status = 200")),
            fingerprint(&with("expected_status = [200, 401]")),
            fingerprint(&with("expected_status = [\"2xx\", 401]")),
            fingerprint(&with("expected_status = \"4xx\"")),
        ];
        let distinct: std::collections::HashSet<&String> = all.iter().collect();
        assert_eq!(distinct.len(), all.len(), "{all:?}");
        // A list is a set: neither its order nor a repeat is a difference.
        assert_eq!(
            fingerprint(&with("expected_status = [401, 200, 401]")),
            fingerprint(&with("expected_status = [200, 401]"))
        );
    }

    #[test]
    fn what_changes_the_verdict_changes_the_fingerprint() {
        let base = fingerprint(API);
        for (what, changed) in [
            ("status", API.replace("200", "204")),
            ("no status", API.replace("expected_status = 200", "")),
            ("keyword", API.replace("\"ok\"", "\"OK\"")),
            ("no keyword", API.replace("keyword = \"ok\"", "")),
            ("inverted", format!("{API}\nkeyword_invert = true")),
            ("host header", API.replace("internal.", "public.")),
            ("credential", API.replace("s3cret", "0ther")),
            ("path", API.replace("/health", "/ready")),
            ("json", format!("{API}\njson_query = \"$.status\"")),
            ("number", format!("{API}\nnumber_regex = \"(\\\\d+)\"")),
            ("body cap", format!("{API}\nmax_body_kb = 4")),
            ("families", format!("{API}\ndual_stack = true")),
        ] {
            assert_ne!(fingerprint(&changed), base, "{what}");
        }
        let json = format!("{API}\njson_query = \"$.status\"");
        assert_ne!(
            fingerprint(&format!("{json}\njson_expected = \"up\"")),
            fingerprint(&json)
        );
        let tcp = "kind = \"tcp\"\ntarget = \"mail.example.com:587\"";
        assert_ne!(
            fingerprint(&format!("{tcp}\nstarttls = \"smtp\"")),
            fingerprint(tcp)
        );
        assert_ne!(
            fingerprint(&format!("{tcp}\nstarttls = \"smtp\"")),
            fingerprint(&format!("{tcp}\nstarttls = \"imap\""))
        );
        let dns = "kind = \"dns\"\ntarget = \"example.com\"";
        assert_ne!(
            fingerprint(&format!("{dns}\ndns_expected = \"1.1.1.1\"")),
            fingerprint(dns)
        );
        assert_ne!(
            fingerprint(&format!("{dns}\ndns_record = \"AAAA\"")),
            fingerprint(dns)
        );
        // The same target under another kind is another check.
        assert_ne!(
            fingerprint("kind = \"icmp\"\ntarget = \"example.com\""),
            fingerprint(dns)
        );
    }

    #[test]
    fn what_is_this_nodes_own_business_does_not() {
        let base = fingerprint(API);
        for (what, changed) in [
            ("timeout", format!("{API}\ntimeout_secs = 3")),
            ("retries", format!("{API}\nprobe_retries = 3")),
            ("threshold", format!("{API}\ndegraded_over_ms = 200")),
            (
                "proxy",
                format!("{API}\nproxy = \"http://proxy.example:3128\""),
            ),
            ("header case", API.replace("Host =", "host =")),
            (
                "header order",
                API.replace(
                    "{ Host = \"internal.example.com\", Authorization = \"Bearer s3cret\" }",
                    "{ Authorization = \"Bearer s3cret\", Host = \"internal.example.com\" }",
                ),
            ),
        ] {
            assert_eq!(fingerprint(&changed), base, "{what}");
        }
        // A pinned record set is a set.
        let dns = "kind = \"dns\"\ntarget = \"example.com\"\ndns_expected = ";
        assert_eq!(
            fingerprint(&format!("{dns}\"1.1.1.1,2.2.2.2\"")),
            fingerprint(&format!("{dns}\"2.2.2.2, 1.1.1.1\""))
        );
        // A secret in the URL is masked before it is compared: a peer never
        // holds it, and must still recognize the check.
        assert_eq!(
            fingerprint("target = \"https://api.example.com/health?api_key=one\""),
            fingerprint("target = \"https://api.example.com/health?api_key=two\"")
        );
    }

    #[test]
    fn a_value_cannot_pass_for_two_fields() {
        // Without the length prefix these two would write the same lines.
        let split =
            fingerprint("target = \"https://example.com\"\nkeyword = \"a\"\njson_query = \"$.b\"");
        let joined = fingerprint(
            "target = \"https://example.com\"\nkeyword = \"a\\nkeyword_invert=5:false\\njson_query=3:$.b\"",
        );
        assert_ne!(split, joined);
    }

    #[test]
    fn push_and_exec_monitors_have_none() {
        let push = monitor("kind = \"push\"\nallow_unauthenticated = true");
        assert!(check_fingerprint(&push).is_none());
    }
}
