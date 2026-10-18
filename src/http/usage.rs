//! Frozen `/v1/usage`: the pool's last-known quota, in the shape a dashboard reads.

use super::{AppState, error_response, json_response};
use crate::quota_report::{ReportError, Scope, usage_report};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// usage
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub(super) struct UsageQuery {
    /// Only the accounts of this provider.
    #[serde(default)]
    provider: Option<String>,
    /// Only this account, named provider/account-id.
    #[serde(default)]
    account: Option<String>,
}

/// The pool's last-known quota, in the shape a dashboard reads.
///
/// Offline by construction: it renders the poller's last readings and calls no
/// provider. A widget refreshing on a timer would otherwise turn its refresh
/// interval into upstream traffic, and against an account metered per request
/// that is a charge for looking at the bill.
///
/// Behind the same local key as every other route. This endpoint describes the
/// pool, and the local key is already what opens the pool.
pub(super) async fn usage(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let accounts = state.backend.snapshot();
    let scope = Scope {
        provider: query.provider.as_deref(),
        account: query.account.as_deref(),
    };
    let report = match usage_report(&accounts, scope) {
        Ok(report) => report,
        Err(err) => return error_response(usage_status(&err), err.kind(), &err.to_string()),
    };
    match serde_json::to_value(&report) {
        Ok(payload) => json_response(payload),
        Err(e) => {
            tracing::error!(error = %e, "the usage report could not be serialised");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the usage report could not be serialised",
            )
        }
    }
}

/// A scope nobody has probed is 503, not an empty 200: there are no numbers
/// yet, and zeros are indistinguishable from an untouched allowance.
fn usage_status(err: &ReportError) -> StatusCode {
    match err {
        ReportError::UnqualifiedAccount(_) => StatusCode::BAD_REQUEST,
        ReportError::UnknownProvider(_) | ReportError::UnknownAccount(_) => StatusCode::NOT_FOUND,
        ReportError::NeverProbed => StatusCode::SERVICE_UNAVAILABLE,
    }
}
