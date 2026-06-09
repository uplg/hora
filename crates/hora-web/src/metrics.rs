//! Prometheus `/metrics` endpoint: renders the summary as Prometheus text
//! exposition format (version 0.0.4).

use std::fmt::Write as _;

use crate::summary::Summary;

pub(crate) fn render(summary: &Summary) -> String {
    let mut out = String::with_capacity(4096);

    push_header(
        &mut out,
        "hora_monitor_up",
        "Whether the monitor is up (degraded still counts as up)",
        "gauge",
    );
    for monitor in &summary.monitors {
        let up = u8::from(monitor.status == "up" || monitor.status == "degraded");
        let _ = writeln!(out, "hora_monitor_up{} {up}", labels(monitor));
    }

    push_header(
        &mut out,
        "hora_monitor_degraded",
        "Whether the monitor is up but slower than its degraded threshold",
        "gauge",
    );
    for monitor in &summary.monitors {
        let degraded = u8::from(monitor.status == "degraded");
        let _ = writeln!(out, "hora_monitor_degraded{} {degraded}", labels(monitor));
    }

    push_header(
        &mut out,
        "hora_monitor_uptime_ratio",
        "Uptime over the last 24 hours (0 to 1)",
        "gauge",
    );
    for monitor in &summary.monitors {
        if let Some(permille) = monitor.uptime_permille {
            // Permille is 0..=1000 by construction; clamp rather than cast.
            let ratio = f64::from(u16::try_from(permille).unwrap_or(1000)) / 1000.0;
            let _ = writeln!(
                out,
                "hora_monitor_uptime_ratio{} {ratio:.3}",
                labels(monitor)
            );
        }
    }

    push_header(
        &mut out,
        "hora_monitor_last_latency_ms",
        "Latency of the most recent check in milliseconds",
        "gauge",
    );
    for monitor in &summary.monitors {
        if let Some(ms) = monitor.last_latency_ms {
            let _ = writeln!(out, "hora_monitor_last_latency_ms{} {ms}", labels(monitor));
        }
    }

    push_header(
        &mut out,
        "hora_monitor_latency_ms",
        "Latency quantiles over the last 24 hours in milliseconds",
        "summary",
    );
    for monitor in &summary.monitors {
        for (quantile, value) in [
            ("0.5", monitor.p50_ms),
            ("0.95", monitor.p95_ms),
            ("0.99", monitor.p99_ms),
        ] {
            if let Some(ms) = value {
                let _ = writeln!(
                    out,
                    "hora_monitor_latency_ms{{id=\"{}\",name=\"{}\",quantile=\"{quantile}\"}} {ms}",
                    escape_label(&monitor.id),
                    escape_label(&monitor.name),
                );
            }
        }
    }

    push_header(
        &mut out,
        "hora_cert_expiry_days",
        "Days until the TLS certificate expires",
        "gauge",
    );
    for monitor in &summary.monitors {
        if let Some(days) = monitor.cert_days {
            let _ = writeln!(out, "hora_cert_expiry_days{} {days}", labels(monitor));
        }
    }

    out
}

fn push_header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn labels(monitor: &crate::summary::MonitorView) -> String {
    format!(
        "{{id=\"{}\",name=\"{}\"}}",
        escape_label(&monitor.id),
        escape_label(&monitor.name)
    )
}

/// Escape a label value per the Prometheus exposition format: backslash,
/// double quote and newline.
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape_label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");
        assert_eq!(escape_label("plain"), "plain");
    }
}
