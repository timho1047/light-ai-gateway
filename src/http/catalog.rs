//! Provider model catalogs: reading, indexing and looking up model ids.
//!
//! Shared by `/v1/models` and `lgw doctor`; a second copy of this would be a
//! second answer about what a provider publishes.

use super::{ProxyBackend, read_bounded, upstream_error_message};
use crate::domain::ApiKey;
use crate::runtime::Runtime;
use axum::http::header;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::BTreeMap;

/// Cap on a provider's model catalog.
///
/// It is a list of names, not a payload: a provider answering with more than this
/// is not answering the question we asked, and reading it unbounded lets one slow
/// upstream hold the endpoint open.
const MAX_CATALOG_BYTES: usize = 1024 * 1024;

/// One provider's own model catalog, keyed by the id that provider publishes.
pub(crate) type Catalog = BTreeMap<String, Value>;

/// The published ids closest to a model that is missing from a catalog, as one
/// line for a log. "-" when nothing is close, so the line still reads.
pub(super) fn closest_hint(catalog: &Catalog, model: &str) -> String {
    let ids = crate::closest_names(catalog.keys().map(String::as_str), model, 8);
    if ids.is_empty() {
        "-".to_string()
    } else {
        ids.join(", ")
    }
}

/// The provider's entry for a model: exact match first, then case-insensitive.
///
/// A provider that spells its own id differently from the route is not a reason
/// to drop the fields we asked for, and a case-insensitive match cannot pick the
/// wrong model -- only the same name spelled differently.
pub(super) fn lookup<'a>(catalog: &'a Catalog, model: &str) -> Option<&'a Value> {
    catalog.get(model).or_else(|| {
        catalog
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(model))
            .map(|(_, entry)| entry)
    })
}

/// Index a `/models` response by model id.
///
/// Both envelopes in the wild are accepted -- OpenAI's
/// `{"object":"list","data":[]}` and a bare array -- because a provider that
/// answers the same question in the other shape is not worth failing a model list
/// over. Entries without a usable id are dropped rather than listed as blank.
fn catalog_from(value: Value) -> Catalog {
    let entries = match value {
        Value::Array(entries) => entries,
        other => other
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.trim().to_string();
            (!id.is_empty()).then_some((id, entry))
        })
        .collect()
}

/// Read one provider's catalog with an eligible account's key, through the pool.
pub(super) async fn catalog_for_provider(
    backend: &dyn ProxyBackend,
    runtime: &Runtime,
    provider: &str,
    request_id: &str,
) -> std::result::Result<Catalog, String> {
    // A model list spends no tokens, so it is planned at zero cost, and
    // pick_account hands the reservation straight back: polling the list cannot
    // fill an account's concurrency limit. (An account with an rpm_limit does
    // count the call, which is the one place a polling client is visible to the
    // pool -- hence one call per provider, not one per model.)
    let plan = backend
        .pick_account(provider, Decimal::ZERO, request_id, 1)
        .map_err(|e| e.to_string())?;
    let Some(account) = backend.account(&plan.qualified_account()) else {
        return Err("the planned account is no longer in the pool".to_string());
    };
    fetch_catalog(&backend.http(runtime), &account.base_url, &account.api_key).await
}

/// Read and parse one provider's model catalog.
///
/// Shared by `/v1/models` and `lgw doctor`: the two have to agree about what a
/// provider publishes, and a second copy of this is a second answer. Errors are
/// strings because every caller reports them rather than acting on them.
pub(crate) async fn fetch_catalog(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &ApiKey,
) -> std::result::Result<Catalog, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .bearer_auth(api_key.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("{url} is unreachable: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let text = read_bounded(response, 8 * 1024).await;
        return Err(upstream_error_message(&text, api_key)
            .unwrap_or_else(|| format!("{url} returned HTTP {}", status.as_u16())));
    }

    let text = read_bounded(response, MAX_CATALOG_BYTES).await;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{url} did not answer with JSON: {e}"))?;
    Ok(catalog_from(value))
}

/// Whether the provider publishes this model id (exact, then case-insensitive).
pub(crate) fn publishes(catalog: &Catalog, model: &str) -> bool {
    lookup(catalog, model).is_some()
}
