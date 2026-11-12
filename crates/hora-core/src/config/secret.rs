//! Secrets in the configuration, and the redaction that keeps them out of logs.

use serde::Deserialize;

/// Render a secret for `Debug`: `<unset>` when empty, `<redacted>` otherwise.
pub(super) fn redacted(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

/// A configuration string that is redacted from `Debug` output, so a secret in a
/// `Debug`-derived struct (e.g. a monitor's push token) never reaches the logs.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(redacted(&self.0))
    }
}

// `AsRef`, not `Deref`: reading the secret must be explicit (`.as_ref()`), so it
// can't be coerced into a `Display` context (e.g. `info!("{}", *secret)`) by
// accident.
impl AsRef<str> for Secret {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Secret {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Mask the secrets a URL-like string may carry for `Debug` - `user:pass@`
/// credentials and query-string values (`?api_key=...`) - keeping the host,
/// path and query *keys* so logs stay useful. Inputs that don't parse as a URL
/// (e.g. a TCP `host:port` target) or carry neither are returned unchanged.
#[must_use]
pub fn redact_url_secrets(raw: &str) -> std::borrow::Cow<'_, str> {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return std::borrow::Cow::Borrowed(raw);
    };
    let mut redacted = false;
    if !url.username().is_empty() || url.password().is_some() {
        // These setters only fail for cannot-be-a-base URLs, which never
        // carry credentials, so the guard above already excludes them.
        let _ = url.set_username("***");
        if url.password().is_some() {
            let _ = url.set_password(Some("***"));
        }
        redacted = true;
    }
    if url.query().is_some_and(|query| !query.is_empty()) {
        // A key comes out of `query_pairs` decoded. The four characters that
        // would read differently the next time (`a%26b` is one key, `a&b`
        // two) are written encoded again, so that masking a masked URL
        // changes nothing: peers compare targets masked on both sides (see
        // `mesh::wire::disclosed_target`).
        let keys: Vec<String> = url
            .query_pairs()
            .map(|(key, _value)| {
                let key = key
                    .replace('%', "%25")
                    .replace('&', "%26")
                    .replace('=', "%3D")
                    .replace('+', "%2B");
                format!("{key}=***")
            })
            .collect();
        url.set_query(Some(&keys.join("&")));
        redacted = true;
    }
    if redacted {
        std::borrow::Cow::Owned(url.to_string())
    } else {
        std::borrow::Cow::Borrowed(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::redact_url_secrets;

    /// Peers compare targets masked on both sides, the one received being
    /// masked a second time: masking what is already masked must change
    /// nothing. Found by the `text` fuzz target: a query key holding an
    /// encoded `&`, `=` or `+` was written back decoded, and read as another
    /// query the second time.
    #[test]
    fn masking_a_masked_url_changes_nothing() {
        for (raw, masked) in [
            ("http://h/?a%26b=1", "http://h/?a%26b=***"),
            ("http://h/?a%3Db", "http://h/?a%3Db=***"),
            ("http://h/?a%2Bb=1", "http://h/?a%2Bb=***"),
            ("http://h/?a%2541=1", "http://h/?a%2541=***"),
            // Everything else is written as it always was.
            (
                "http://h/?api_key=s3cret&x=1",
                "http://h/?api_key=***&x=***",
            ),
            ("http://h/?a+b=1&c%20d=2", "http://h/?a%20b=***&c%20d=***"),
            ("http://h/?filter[x]=1", "http://h/?filter[x]=***"),
            ("https://u:p@h/p?k", "https://***:***@h/p?k=***"),
        ] {
            let once = redact_url_secrets(raw);
            assert_eq!(once, masked, "{raw}");
            assert_eq!(redact_url_secrets(&once), masked, "{raw}, masked twice");
        }
    }
}
