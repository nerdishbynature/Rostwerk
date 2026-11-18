//! Dry runs of write endpoints: run the real code path, then roll back.
//!
//! A client sends `Prefer: dry-run` (RFC 7240). The handler runs as usual:
//! validation, database constraints, triggers and events all happen inside the
//! tenant transaction. At the end the transaction is rolled back instead of
//! committed, so nothing is persisted: no rows, no audit entries, no outbox
//! events. The response has the status the real call would return, the header
//! `Preference-Applied: dry-run`, and the body
//!
//! ```json
//! { "result": <the normal response body>, "events": [{ "topic": "...", "payload": {...} }] }
//! ```
//!
//! Errors are returned exactly as for a real call.
//!
//! A write handler supports it by taking [`DryRun`] and finishing with
//! [`DryRun::respond`] instead of committing itself:
//!
//! ```ignore
//! #[utoipa::path(post, path = "/v1/things", params(DryRun), ...)]
//! pub async fn create(State(state): State<CoreState>, ctx: TenantCtx, dry_run: DryRun,
//!                     Json(new): Json<NewThing>) -> Result<Response> {
//!     let mut tx = ctx.begin(&state.db).await?;
//!     let thing = /* insert */;
//!     tx.publish("module.thing.created", &thing).await?;
//!     dry_run.respond(tx, StatusCode::CREATED, thing).await
//! }
//! ```
//!
//! Endpoints that do not take [`DryRun`] ignore the preference, as RFC 7240
//! allows, and do not send `Preference-Applied`. Clients must check for it.

use axum::Json;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use core_kernel::{RecordedEvent, Result, TenantTx};
use serde::Serialize;
use utoipa::IntoParams;
use utoipa::openapi::Required;
use utoipa::openapi::path::{Parameter, ParameterBuilder, ParameterIn};
use utoipa::openapi::schema::{ObjectBuilder, Type};

/// Request header carrying client preferences (RFC 7240).
pub const PREFER: HeaderName = HeaderName::from_static("prefer");
/// The preference token, in `Prefer` and `Preference-Applied`.
pub const DRY_RUN: &str = "dry-run";
/// Response header confirming that a preference was honoured (RFC 7240).
pub const PREFERENCE_APPLIED: HeaderName = HeaderName::from_static("preference-applied");

/// Whether the request asked for a dry run. Extract it in write handlers and
/// finish with [`DryRun::respond`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DryRun(bool);

impl DryRun {
    /// `true` if the client sent `Prefer: dry-run`.
    pub fn requested(self) -> bool {
        self.0
    }

    /// Reads `Prefer` headers. Several preferences may be given, comma-separated
    /// or in several headers; tokens are case-insensitive, parameters after `;`
    /// are ignored.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let requested = headers
            .get_all(PREFER)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|pref| pref.split(';').next().unwrap_or_default().trim())
            .any(|token| token.eq_ignore_ascii_case(DRY_RUN));
        Self(requested)
    }

    /// Finishes a write: commits `tx` and returns `body` with `status`, or, in
    /// a dry run, rolls `tx` back and returns the dry-run response.
    pub async fn respond<T: Serialize>(
        self,
        tx: TenantTx,
        status: StatusCode,
        body: T,
    ) -> Result<Response> {
        let events = tx.finish(self.0).await?;
        if !self.0 {
            return Ok((status, Json(body)).into_response());
        }
        let mut response = (
            status,
            Json(DryRunResponse {
                result: body,
                events,
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(PREFERENCE_APPLIED, HeaderValue::from_static(DRY_RUN));
        Ok(response)
    }
}

#[derive(Serialize)]
struct DryRunResponse<T> {
    result: T,
    events: Vec<RecordedEvent>,
}

impl<S: Send + Sync> FromRequestParts<S> for DryRun {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(Self::from_headers(&parts.headers))
    }
}

/// Documents the `Prefer` header. Add `params(DryRun)` to the endpoint's `#[utoipa::path]`.
impl IntoParams for DryRun {
    fn into_params(_parameter_in: impl Fn() -> Option<ParameterIn>) -> Vec<Parameter> {
        vec![
            ParameterBuilder::new()
                .name("Prefer")
                .parameter_in(ParameterIn::Header)
                .required(Required::False)
                .description(Some(
                    "`dry-run`: validate and compute the result without saving anything. \
                     The response then carries `Preference-Applied: dry-run`, the status of \
                     the real call and the body `{\"result\": <normal body>, \"events\": \
                     [{\"topic\", \"payload\"}]}` with the events that would be published.",
                ))
                .schema(Some(
                    ObjectBuilder::new()
                        .schema_type(Type::String)
                        .enum_values(Some([DRY_RUN])),
                ))
                .build(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> bool {
        let mut headers = HeaderMap::new();
        for v in values {
            headers.append(PREFER, HeaderValue::from_str(v).unwrap());
        }
        DryRun::from_headers(&headers).requested()
    }

    #[test]
    fn parses_prefer_header() {
        assert!(!parse(&[]));
        assert!(parse(&["dry-run"]));
        assert!(parse(&["Dry-Run"]));
        assert!(parse(&["return=minimal, dry-run"]));
        assert!(parse(&["respond-async", " dry-run ; x=1"]));
        assert!(!parse(&["dry-runs"]));
        assert!(!parse(&["return=dry-run"]));
    }
}
