//! `${VAR}` expansion of the config's string values.

use std::env::VarError;

/// Where a variable's value comes from: the process environment in
/// production, a fixed table in tests (which must not mutate the environment).
pub(super) type Lookup<'a> = &'a dyn Fn(&str) -> Result<String, VarError>;

/// The process environment, as a [`Lookup`].
pub(super) fn process_env(name: &str) -> Result<String, VarError> {
    std::env::var(name)
}

/// What is wrong with one `${...}` reference. None of these fails the load:
/// the config can be fixed and reloaded while the daemon runs, so each is a
/// warning (which `hora check` prints and counts). The messages name the
/// variable, never the fallback or the surrounding text: either may be a
/// secret.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExpandWarning {
    /// `${NAME}` without a fallback, and `NAME` is not in the environment.
    /// Expands to nothing.
    Unset(String),
    /// `NAME` is set to bytes that are not Unicode text. Expands to nothing.
    NotUnicode(String),
    /// What stands between the braces is not a variable name. Left as written.
    NotAName(String),
    /// The fallback of `${NAME:-...}` holds a `{`. Left as written.
    FallbackBrace(String),
}

impl std::fmt::Display for ExpandWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unset(name) => write!(
                f,
                "environment variable {name} is not set (write ${{{name}:-}} if it is \
                 optional, or ${{{name}:-fallback}} for a default)"
            ),
            Self::NotUnicode(name) => {
                write!(f, "environment variable {name} is not valid Unicode")
            }
            Self::NotAName(name) => write!(
                f,
                "${{{name}}} does not name an environment variable (letters, digits \
                 and '_', as ${{NAME}} or ${{NAME:-fallback}}): left as written; \
                 write $${{ for a literal ${{"
            ),
            Self::FallbackBrace(name) => write!(
                f,
                "the fallback of ${{{name}:-...}} must not contain '{{' or '}}' (the first \
                 '}}' ends the reference): left as written, set {name} instead"
            ),
        }
    }
}

/// Expand `${VAR}` in every string value of the document, recursively. Keys,
/// comments and non-string values are never touched, so a `${VAR}` in a
/// commented-out example neither warns nor pulls a secret into the file's
/// parse context.
///
/// Returns one line per reference that could not be resolved, behind the
/// path of the key whose value holds it (`channels[0].token: ...`), for the
/// caller to log: the key is what tells a typo'd variable from the right one.
#[must_use]
pub(super) fn expand_env_table(table: &mut toml::Table, lookup: Lookup<'_>) -> Vec<String> {
    let mut warnings = Vec::new();
    expand_table(table, "", lookup, &mut warnings);
    warnings
}

fn expand_table(table: &mut toml::Table, path: &str, lookup: Lookup<'_>, out: &mut Vec<String>) {
    for (key, value) in table.iter_mut() {
        let path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        expand_value(value, &path, lookup, out);
    }
}

fn expand_value(value: &mut toml::Value, path: &str, lookup: Lookup<'_>, out: &mut Vec<String>) {
    match value {
        toml::Value::String(text) => {
            if text.contains('$') {
                let mut warnings = Vec::new();
                *text = expand_env(text, lookup, &mut warnings);
                out.extend(
                    warnings
                        .into_iter()
                        .map(|warning| format!("{path}: {warning}")),
                );
            }
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                expand_value(item, &format!("{path}[{index}]"), lookup, out);
            }
        }
        toml::Value::Table(table) => expand_table(table, path, lookup, out),
        _ => {}
    }
}

/// Substitute `${VAR}` with the variable's value and `${VAR:-fallback}` with
/// the fallback when the variable is unset or empty (the shell's rule). `$$`
/// is a literal `$`, so `$${id}` yields a literal `${id}`. Runs on
/// already-parsed string values, so the result needs no TOML escaping - a
/// value with quotes or newlines is stored verbatim and can't break parsing
/// or inject config.
///
/// Nothing here fails: an unset variable without a fallback expands to
/// nothing, as it always did, and is reported in `warnings` - with the key,
/// by the caller - so that the typo which empties a channel's secret is said
/// where it is made. `${VAR:-}` is how a value says it may be absent, and
/// warns about nothing.
///
/// The fallback is literal text up to the first `}`: no `$$`, no nested
/// reference. One holding a `{` is what a fallback cut short by its own `}`
/// looks like (`${A:-${B}}`, `${A:-{"k":1}}`): that reference, like one whose
/// name is no variable name, is left as written and reported.
pub(super) fn expand_env(
    input: &str,
    lookup: Lookup<'_>,
    warnings: &mut Vec<ExpandWarning>,
) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$'); // `$$` escape.
            rest = stripped;
        } else if let Some(body) = after.strip_prefix('{') {
            if let Some(end) = body.find('}') {
                let reference = &body[..end];
                match resolve(reference, lookup) {
                    Ok(value) => out.push_str(&value),
                    Err(warning) => {
                        if matches!(
                            warning,
                            ExpandWarning::NotAName(_) | ExpandWarning::FallbackBrace(_)
                        ) {
                            out.push_str("${");
                            out.push_str(reference);
                            out.push('}');
                        }
                        warnings.push(warning);
                    }
                }
                rest = &body[end + 1..];
            } else {
                out.push_str("${"); // No closing brace: emit literally.
                rest = body;
            }
        } else {
            out.push('$'); // A lone `$`.
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The value of one reference, given what stands between its braces.
fn resolve(reference: &str, lookup: Lookup<'_>) -> Result<String, ExpandWarning> {
    let (name, fallback) = match reference.split_once(":-") {
        Some((name, fallback)) => (name, Some(fallback)),
        None => (reference, None),
    };
    if !is_variable_name(name) {
        return Err(ExpandWarning::NotAName(name.to_owned()));
    }
    if fallback.is_some_and(|fallback| fallback.contains('{')) {
        return Err(ExpandWarning::FallbackBrace(name.to_owned()));
    }
    match (lookup(name), fallback) {
        (Err(VarError::NotUnicode(_)), _) => Err(ExpandWarning::NotUnicode(name.to_owned())),
        (Ok(value), Some(fallback)) if value.is_empty() => Ok(fallback.to_owned()),
        (Ok(value), _) => Ok(value),
        (Err(VarError::NotPresent), Some(fallback)) => Ok(fallback.to_owned()),
        (Err(VarError::NotPresent), None) => Err(ExpandWarning::Unset(name.to_owned())),
    }
}

/// A shell-style variable name: letters, digits and `_`, not starting with a
/// digit. Anything else between the braces (`${not a variable}` in a body
/// assertion, `${VAR:?}` from a compose file) is not looked up under a name
/// no environment holds.
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reading back what a piece of code warned about, for the tests of the
/// warnings that replace a silent default (an unset variable, a channel left
/// without its secret).
#[cfg(test)]
pub(crate) mod capture {
    use std::sync::{Arc, Mutex};

    use tracing::span;

    /// Run `run` and return, with its result, the message of every `WARN` or
    /// `ERROR` event it emitted on this thread.
    pub(crate) fn captured_warnings<T>(run: impl FnOnce() -> T) -> (T, Vec<String>) {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let result = tracing::subscriber::with_default(Capture(Arc::clone(&collected)), run);
        let warnings = std::mem::take(&mut *collected.lock().expect("capture lock"));
        (result, warnings)
    }

    struct Capture(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Capture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            // `Level` orders by verbosity: ERROR < WARN < INFO.
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _attributes: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().expect("capture lock").push(message.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }

    struct Message(String);

    impl tracing::field::Visit for Message {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }
}
