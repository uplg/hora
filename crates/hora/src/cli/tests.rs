use hora_core::MAX_EVENT_TITLE_CHARS;
use hora_core::announce;

use super::annotations::{EventCommand, parse_announce_args, parse_event_args};
use super::probe::parse_probe_args;
use super::*;

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|&part| part.to_owned()).collect()
}

#[test]
fn announce_args_split_flags_title_and_body() {
    let announcement = parse_announce_args(
        &strings(&[
            "Fiber",
            "cut,",
            "ETA",
            "6pm",
            "--severity",
            "warning",
            "--until",
            "4h",
        ]),
        1000,
    )
    .expect("parse");
    assert_eq!(announcement.title, "Fiber");
    assert_eq!(announcement.body, "cut, ETA 6pm");
    assert_eq!(announcement.severity, announce::Severity::Warning);
    assert_eq!(announcement.until, Some(1000 + 4 * 3600));

    assert!(parse_announce_args(&strings(&["--severity", "warning"]), 0).is_err());
    assert!(parse_announce_args(&strings(&["t", "--severity", "panic"]), 0).is_err());
    assert!(parse_announce_args(&strings(&["t", "--until", "nope"]), 0).is_err());
}

/// The message a usage error would print, or a panic for any other outcome.
fn usage_message<T: std::fmt::Debug>(result: Result<T, CliError>) -> String {
    match result {
        Err(CliError::Usage(message)) => message,
        other => panic!("expected a usage error, got {other:?}"),
    }
}

fn two_monitor_config() -> hora_core::config::Config {
    hora_core::config::parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "Public API"
            target = "https://api.example.com"
            interval_secs = 60
            [[monitors]]
            id = "db"
            name = "Database"
            target = "db.internal:5432"
            kind = "tcp"
            interval_secs = 60
            "#,
    )
    .expect("config")
}

#[test]
fn probe_args_return_usage_errors_instead_of_exiting() {
    let args = strings(&["example.com", "--confirm", "--kind", "ping"]);
    let parsed = parse_probe_args(&args).expect("parse");
    assert_eq!(parsed.target, "example.com");
    assert!(parsed.confirm);
    assert_eq!(parsed.kind_override, Some(hora_core::config::Kind::Icmp));

    assert!(usage_message(parse_probe_args(&[])).starts_with("Usage: hora probe"));
    assert!(usage_message(parse_probe_args(&strings(&["a", "b"]))).contains("single target"));
    assert!(usage_message(parse_probe_args(&strings(&["--nope"]))).contains("Unknown option"));
    assert!(
        usage_message(parse_probe_args(&strings(&["a", "--kind", "smtp"])))
            .starts_with("--kind must be")
    );
}

#[test]
fn probe_takes_a_method_and_statuses_for_an_ad_hoc_http_target() {
    use hora_core::config::{HttpMethod, Kind, Monitor, MonitorKind};

    let args = strings(&[
        "https://example.com",
        "--method",
        "head",
        "--expect",
        "2xx,401",
    ]);
    let parsed = parse_probe_args(&args).expect("parse");
    let mut monitor = Monitor::ad_hoc(Kind::Http, parsed.target.to_owned()).unwrap();
    super::probe::apply_http_options(&mut monitor, &parsed, false).expect("an ad-hoc http target");
    let MonitorKind::Http(spec) = &monitor.spec else {
        panic!("http");
    };
    assert_eq!(spec.method, HttpMethod::Head);
    let expected = spec.expected_status.as_ref().expect("set");
    assert!(expected.matches(401) && expected.matches(204) && !expected.matches(403));

    // A configured monitor is probed as configured, and a ping has neither.
    let error = usage_message(super::probe::apply_http_options(
        &mut monitor,
        &parsed,
        true,
    ));
    assert!(error.contains("own method and expected_status"), "{error}");
    let mut ping = Monitor::ad_hoc(Kind::Icmp, "192.0.2.1".to_owned()).unwrap();
    let error = usage_message(super::probe::apply_http_options(&mut ping, &parsed, false));
    assert!(error.contains("need an http target"), "{error}");

    let bad = |args: &[&str]| usage_message(parse_probe_args(&strings(args)));
    assert!(bad(&["a", "--method", "POST"]).starts_with("--method: "));
    assert!(bad(&["a", "--expect", "200,abc"]).starts_with("--expect: "));
    assert!(bad(&["a", "--expect"]).starts_with("--expect: "));
}

#[test]
fn event_args_disambiguate_list_from_a_title() {
    assert_eq!(
        parse_event_args(&strings(&["list"])).expect("list"),
        EventCommand::List(20)
    );
    assert_eq!(
        parse_event_args(&strings(&["list", "0"])).expect("list 0"),
        EventCommand::List(1)
    );
    assert_eq!(
        parse_event_args(&strings(&["deploy", "api", "v2.3"])).expect("record"),
        EventCommand::Record("deploy api v2.3".to_owned())
    );
    // `list <words>` is ambiguous: refuse, and point at the `--` escape.
    let message = usage_message(parse_event_args(&strings(&["list", "deploy"])));
    assert!(message.contains("hora event -- list deploy"), "{message}");
    assert_eq!(
        parse_event_args(&strings(&["--", "list", "deploy"])).expect("escaped"),
        EventCommand::Record("list deploy".to_owned())
    );
    // Titles are capped like the API's; empty ones are refused.
    let long = "x".repeat(300);
    assert_eq!(
        parse_event_args(&strings(&[&long])).expect("long"),
        EventCommand::Record("x".repeat(MAX_EVENT_TITLE_CHARS))
    );
    assert!(usage_message(parse_event_args(&[])).starts_with("Usage: hora event"));
    usage_message(parse_event_args(&strings(&["--"])));
    usage_message(parse_event_args(&strings(&["  "])));
}

#[test]
fn unknown_monitor_lists_the_configured_ids() {
    let config = two_monitor_config();
    assert_eq!(find_monitor(&config, "db").expect("db").name, "Database");
    assert_eq!(
        usage_message(find_monitor(&config, "dbb")),
        "Unknown monitor \"dbb\". Configured ids:\n  api\n  db"
    );
    assert_eq!(config.monitor_name("api"), "Public API");
    // A monitor removed since its incident was recorded keeps its id.
    assert_eq!(config.monitor_name("gone"), "gone");
}

#[test]
fn limit_and_days_flags_validate() {
    assert_eq!(parse_limit(None, "u").expect("default"), 20);
    assert_eq!(
        parse_limit(Some(&"-5".to_owned()), "u").expect("clamped"),
        1
    );
    assert_eq!(usage_message(parse_limit(Some(&"x".to_owned()), "u")), "u");
    assert_eq!(parse_days(Some(&"3".to_owned())).expect("days"), 3);
    usage_message(parse_days(Some(&"0".to_owned())));
    usage_message(parse_days(None));
}

#[tokio::test]
async fn incident_ids_resolve_last_or_a_number() {
    let store = hora_core::db::Store::in_memory().await;
    assert_eq!(resolve_incident_id(&store, "42").await.expect("number"), 42);
    assert!(
        usage_message(resolve_incident_id(&store, "4x2").await).contains("a number, or 'last'")
    );
    match resolve_incident_id(&store, "last").await {
        Err(CliError::Failed(message)) => assert_eq!(message, "No incidents recorded yet."),
        other => panic!("expected a failure on an empty database, got {other:?}"),
    }
}

#[test]
fn maintenance_plan_takes_its_flags_anywhere_and_the_rest_as_the_reason() {
    use super::annotations::{PlanArgs, parse_plan_args};

    let args = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
    // 2027-01-15 08:00:00 UTC.
    let now = 1_800_000_000;
    let day_start = now - 8 * 3600;

    // The host script's call: a full timestamp, so a repeat is the same plan.
    let plan = parse_plan_args(
        &args("--host frank --at 2027-01-16T05:00:00Z --for 10m kernel reboot"),
        now,
    )
    .expect("parsed");
    assert_eq!(
        plan,
        PlanArgs {
            host: Some("frank".to_owned()),
            monitors: None,
            starts_at: Some(day_start + 86_400 + 5 * 3600),
            duration: "10m".to_owned(),
            reason: "kernel reboot".to_owned(),
        }
    );
    // At a shell: a clock time is its next occurrence (UTC), flags anywhere.
    let plan = parse_plan_args(
        &args("disk swap --monitors api,web --for 1h --at 05:00"),
        now,
    )
    .expect("parsed");
    assert_eq!(plan.starts_at, Some(day_start + 86_400 + 5 * 3600));
    assert_eq!(plan.monitors.as_deref(), Some("api,web"));
    assert_eq!(plan.reason, "disk swap");
    let plan =
        parse_plan_args(&args("--host frank --for 10m --at 18:30 reboot"), now).expect("parsed");
    assert_eq!(plan.starts_at, Some(day_start + 18 * 3600 + 1800));
    // Unix seconds; and without --at, now (left to the planner).
    let plan = parse_plan_args(&args("--host frank --for 10m --at 1800003600 reboot"), now)
        .expect("parsed");
    assert_eq!(plan.starts_at, Some(now + 3600));
    let plan = parse_plan_args(&args("--host frank --for 10m reboot"), now).expect("parsed");
    assert_eq!(plan.starts_at, None);

    // No duration, a flag without its value, or a start that is not one.
    for bad in [
        "--host frank reboot",
        "--host frank --for",
        "--host frank --for 10m --at soon reboot",
        "--host frank --for 10m --at 2h reboot",
        "--host frank --for 10m --at 25:00 reboot",
    ] {
        assert!(
            matches!(parse_plan_args(&args(bad), now), Err(CliError::Usage(_))),
            "{bad}"
        );
    }
}
