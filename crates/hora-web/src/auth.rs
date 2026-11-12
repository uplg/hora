//! Request authentication: the credential helpers and the axum extractors
//! every handler starts from, so the "who is asking" preamble lives once.

use std::future::Future;
use std::sync::Arc;

use axum::extract::{FromRequestParts, MatchedPath, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower_governor::key_extractor::KeyExtractor;

use hora_core::config::Config;

use crate::error::AppError;
use crate::visibility::Audience;
use crate::{AppState, ConfiguredIp};

pub(crate) use hora_core::mesh::wire::PUSH_TOKEN_HEADER;

/// The `Authorization: Bearer <token>` credential, if any.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token)
}

/// The token of an `Authorization` value with the `Bearer` scheme. The
/// scheme's name is case-insensitive (RFC 9110): `bearer` and `BEARER`
/// name it too. The token itself is compared as sent.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim_start_matches(' '))
        .filter(|token| !token.is_empty())
}

/// The `X-Push-Token` credential, if any.
pub(crate) fn push_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(PUSH_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
}

/// Constant-time string comparison so a wrong token can't be brute-forced by
/// timing. The length may leak (it is not the secret).
pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether `provided` matches the optional `expected` secret. No expected
/// secret never matches.
/// Whether the request's `X-Push-Token` is `expected` (constant-time).
pub(crate) fn push_token_is(headers: &HeaderMap, expected: &impl AsRef<str>) -> bool {
    matches(push_token(headers), Some(expected))
}

fn matches(provided: Option<&str>, expected: Option<&impl AsRef<str>>) -> bool {
    match (provided, expected) {
        (Some(provided), Some(expected)) => ct_eq(provided, expected.as_ref()),
        _ => false,
    }
}

#[derive(Debug, Default, Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

/// What a write answers to a `?token=`, whatever the token.
const QUERY_TOKEN_REFUSED: &str = "?token= is not accepted on write endpoints: send the token \
    in the Authorization: Bearer header (or X-Push-Token for a push or peer token)";

/// Whether the query string carries a `token` parameter, valid or not.
/// Decoded like the extractors decode it, so `%74oken=` is one too. A query
/// that does not parse yields no token to any extractor either.
fn has_query_token(uri: &Uri) -> bool {
    uri.query().is_some_and(|query| !query.is_empty())
        && Query::<Vec<(String, String)>>::try_from_uri(uri)
            .is_ok_and(|Query(pairs)| pairs.iter().any(|(key, _)| key == "token"))
}

/// Whether the request presents a credential at all: `Authorization`,
/// `X-Push-Token` or `?token=`.
pub(crate) fn presents_credential<B>(request: &axum::http::Request<B>) -> bool {
    request.headers().contains_key(header::AUTHORIZATION)
        || request.headers().contains_key(PUSH_TOKEN_HEADER)
        || has_query_token(request.uri())
}

/// API middleware: a write (anything but `GET`/`HEAD`) never takes its
/// credential from the URL. A token in a query string ends up in access
/// logs, proxies and shell histories, and on a write that token can mute an
/// alert; so the request is refused outright - even when the token is right,
/// even next to a valid header - rather than served with a hint nobody reads.
/// The read-only views keep `?token=` (a kiosk cannot set a header).
pub(crate) async fn refuse_query_token(request: Request, next: Next) -> Response {
    let read_only = matches!(*request.method(), Method::GET | Method::HEAD);
    if !read_only && has_query_token(request.uri()) {
        return AppError::Unauthorized(QUERY_TOKEN_REFUSED).into_response();
    }
    next.run(request).await
}

/// Log a failed authentication: a request that presented a credential and
/// was answered 401. The line names the client address (the rate limiter's
/// key, so an IPv6 /64) and the matched route - never the credential, nor
/// the query string that may carry it. It runs behind the rate limiter, so a
/// guessing loop logs at most what the limiter lets through; a request
/// without a credential costs one header lookup.
pub(crate) async fn log_failed_auth(
    State(key): State<ConfiguredIp>,
    request: Request,
    next: Next,
) -> Response {
    let presented = presents_credential(&request).then(|| {
        (
            key.extract(&request).ok(),
            request.method().clone(),
            request.extensions().get::<MatchedPath>().cloned(),
        )
    });
    let response = next.run(request).await;
    if response.status() == StatusCode::UNAUTHORIZED
        && let Some((client, method, route)) = presented
    {
        let client = client.map_or_else(|| "unknown".to_owned(), |ip| ip.to_string());
        let route = route.as_ref().map_or("unmatched", MatchedPath::as_str);
        tracing::warn!(%client, %method, route, "authentication failed");
    }
    response
}

/// The caller of a request: the config snapshot it is served from and the
/// audience its credential (Bearer, else `?token=`) resolves to -
/// `server.auth_token` is the operator, `server.view_token` the read-only
/// [`Audience::Reader`]. With neither configured nothing is private (config
/// validation enforces that), so every caller is [`Audience::Public`].
///
/// On a write the query never reaches here with a token (see
/// [`refuse_query_token`]), so there the credential is the header's.
pub(crate) struct Viewer {
    pub(crate) config: Arc<Config>,
    pub(crate) audience: Audience,
    /// The presented credential (Bearer first, then `?token=`), kept so a
    /// group page can check it against its own group token.
    token: Option<String>,
}

impl Viewer {
    fn from_parts(parts: &Parts, config: Arc<Config>) -> Self {
        // A write never reads its credential from the URL: the middleware
        // already refused the request, and this keeps it true wherever a
        // handler is mounted.
        let read_only = matches!(parts.method, Method::GET | Method::HEAD);
        let query_token = Query::<TokenQuery>::try_from_uri(&parts.uri)
            .ok()
            .filter(|_| read_only)
            .and_then(|Query(query)| query.token);
        let bearer = bearer(&parts.headers).map(str::to_owned);
        let presented = |expected: Option<&hora_core::config::Secret>| {
            matches(bearer.as_deref(), expected) || matches(query_token.as_deref(), expected)
        };
        // The operator token first: config validation keeps the two apart,
        // and if it ever did not, the stronger reading is the configured one.
        let audience = if presented(config.server.auth_token.as_ref()) {
            Audience::Operator
        } else if presented(config.server.view_token.as_ref()) {
            Audience::Reader
        } else {
            Audience::Public
        };
        // In-page links carry the `?token=` themselves (see `layout::Chrome`).
        Self {
            config,
            audience,
            token: bearer.or(query_token),
        }
    }

    /// Whether the caller holds `server.auth_token`: the only audience that
    /// may write, and the only one shown the operator's streams.
    pub(crate) fn is_operator(&self) -> bool {
        self.audience == Audience::Operator
    }

    /// The audience on `group`'s own pages: the operator stays the operator
    /// and the read-only token keeps its view of everything, the group's
    /// `server.group_tokens` entry yields [`Audience::Group`], and anyone
    /// else is public. A group token is never accepted anywhere else.
    pub(crate) fn audience_for_group(&self, group: &str) -> Audience {
        if matches!(self.audience, Audience::Operator | Audience::Reader) {
            return self.audience.clone();
        }
        let expected = self.config.server.group_tokens.get(group);
        if matches(self.token.as_deref(), expected) {
            Audience::Group(group.to_owned())
        } else {
            Audience::Public
        }
    }
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = std::convert::Infallible;

    // Synchronous: everything is in the request head and the config snapshot.
    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let config = state.config.borrow().clone();
        std::future::ready(Ok(Self::from_parts(parts, config)))
    }
}

/// An operator action (announce, silence, event): requires the configured
/// `server.auth_token`, as `Authorization: Bearer`. Without one configured the
/// endpoint is closed - unlike the read-only views, where "no token" just
/// means "everything is public". The read-only `server.view_token` is refused
/// like any other wrong token: reading the page is all it is for.
pub(crate) struct Operator {
    pub(crate) config: Arc<Config>,
}

impl FromRequestParts<AppState> for Operator {
    type Rejection = AppError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let viewer = Viewer::from_parts(parts, state.config.borrow().clone());
        std::future::ready(if viewer.is_operator() {
            Ok(Self {
                config: viewer.config,
            })
        } else {
            Err(AppError::Unauthorized(
                "this endpoint requires server.auth_token, sent as Authorization: Bearer",
            ))
        })
    }
}

/// Authenticate a mesh peer: it must be configured here, it must have a
/// `listen_token` (the id alone never authorizes), and `X-Push-Token` must
/// match. Unknown peers answer exactly like a wrong token. A function rather
/// than an extractor because `/api/peer/probe` names the peer in its JSON body.
pub(crate) fn authorize_peer(
    config: &Config,
    headers: &HeaderMap,
    from: &str,
) -> Result<(), AppError> {
    let expected = config
        .peers
        .iter()
        .find(|peer| peer.id == from)
        .and_then(|peer| peer.listen_token.as_ref());
    if matches(push_token(headers), expected) {
        Ok(())
    } else {
        Err(AppError::Unauthorized("unknown peer or invalid token"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compared_byte_for_byte() {
        assert!(ct_eq("s3cret-token", "s3cret-token"));
        assert!(!ct_eq("s3cret-token", "s3cret-tokeN"));
        assert!(!ct_eq("s3cret-token", "s3cret-toke"));
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
        // Differences that would cancel out if they were summed by XOR.
        assert!(!ct_eq("aa", "bb"));
        assert!(!ct_eq("abab", "baba"));
    }

    #[test]
    fn the_bearer_scheme_is_read_whatever_its_case() {
        for value in [
            "Bearer s3cret",
            "bearer s3cret",
            "BEARER s3cret",
            "Bearer  s3cret",
        ] {
            assert_eq!(bearer_token(value), Some("s3cret"), "{value}");
        }
        // Another scheme, no token, or the scheme glued to the token.
        for value in [
            "Basic s3cret",
            "Bearer",
            "Bearer ",
            "Bearers3cret",
            "s3cret",
        ] {
            assert_eq!(bearer_token(value), None, "{value}");
        }
        // The token is the secret: its case is its own.
        assert_eq!(bearer_token("bearer S3cret"), Some("S3cret"));
    }

    #[test]
    fn a_query_token_is_found_wherever_it_sits() {
        let uri = |query: &str| format!("/api/event?{query}").parse::<Uri>().unwrap();
        for query in [
            "token=x",
            "title=a&token=x",
            "token",
            "a=1&token=&b=2",
            "%74oken=x",
        ] {
            assert!(has_query_token(&uri(query)), "{query}");
        }
        for query in ["", "title=token", "mytoken=x", "tokens=x"] {
            assert!(!has_query_token(&uri(query)), "{query}");
        }
        assert!(!has_query_token(&Uri::from_static("/api/event")));
    }
}
