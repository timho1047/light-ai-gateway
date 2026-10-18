//! `/v1/models`: the configured routes described by each provider's own catalog.

use super::catalog::{Catalog, catalog_for_provider, closest_hint, lookup};
use super::{AppState, error_response, json_response};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(serde::Deserialize)]
pub(super) struct ModelsQuery {
    /// Only list the routes served by this provider.
    #[serde(default)]
    provider: Option<String>,
}

/// The model catalog: exactly the routes this gateway is configured to serve.
///
/// The SET is ours and the FIELDS are the provider's. A client's contract with
/// this gateway is `[routes]` -- `Config::resolve` refuses anything else with "no
/// route for model", so answering with the provider's whole catalog would
/// advertise a hundred models that all fail, and the one thing a model list is
/// for is picking one that works. Each listed model therefore comes from the
/// route table, while everything else about it (display name, context length,
/// pricing, capabilities -- whatever the provider publishes) is passed through
/// from that provider's own `/v1/models` response. Nothing here is a second copy
/// that can drift from what the provider charges.
///
/// One upstream call per provider, not per model. A provider whose catalog cannot
/// be read does not fail the request either: its configured routes are listed
/// with the fields we do have, the reason is logged, and a client still gets the
/// list it needs.
pub(super) async fn list_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ModelsQuery>,
) -> Response {
    // One snapshot for the whole list: the routes and provider views it renders
    // come from the configuration that admitted it.
    let runtime = state.request.runtime();
    let backend = state.request.as_ref();

    if let Some(named) = &query.provider
        && !runtime.config().providers.contains_key(named)
    {
        return error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            &format!("unknown provider {named}"),
        );
    }

    // routes is model -> provider, and it iterates in model order, which is the
    // order a reader scans a model list in.
    let routes: Vec<(String, String)> = runtime
        .config()
        .routes
        .iter()
        .filter(|(_, provider)| {
            query
                .provider
                .as_ref()
                .is_none_or(|named| named == *provider)
        })
        .map(|(model, provider)| (model.clone(), provider.clone()))
        .collect();

    let mut wanted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (model, provider) in &routes {
        wanted.entry(provider.as_str()).or_default().push(model);
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut catalogs: BTreeMap<&str, Catalog> = BTreeMap::new();
    for provider in wanted.keys() {
        match catalog_for_provider(backend, &runtime, provider, &request_id).await {
            Ok(catalog) => {
                let unmatched: Vec<&str> = wanted[provider]
                    .iter()
                    .copied()
                    .filter(|model| lookup(&catalog, model).is_none())
                    .collect();
                for model in unmatched {
                    // A model list is read for its fields, so a route the provider
                    // does not publish is a route with no fields -- and the first
                    // question is always "what does the provider call it?". The ids
                    // answer that. Debug level, names only.
                    tracing::debug!(
                        provider,
                        model,
                        published = catalog.len(),
                        closest = %closest_hint(&catalog, model),
                        "this configured route is not in the provider's catalog; it is \
                         listed without the provider's fields"
                    );
                }
                catalogs.insert(provider, catalog);
            }
            Err(reason) => tracing::warn!(
                provider,
                reason = %reason,
                "the model catalog could not be read; listing this provider's configured \
                 routes without the provider's own fields"
            ),
        }
    }

    let mut data: Vec<Value> = Vec::with_capacity(routes.len());
    for (model, provider) in &routes {
        let published = catalogs
            .get(provider.as_str())
            .and_then(|catalog| lookup(catalog, model));
        data.push(model_entry(model, provider, published));
    }

    json_response(json!({ "object": "list", "data": data }))
}

/// One model: the provider's entry, with the two fields that are ours.
///
/// `id` and `object` are always set here -- the client asked for this route's
/// name, and an entry without `object` is not a model object. `owned_by` names
/// the provider only when the provider's own entry does not: when it is there it
/// is the provider's answer, not ours, and it is never overwritten.
fn model_entry(model: &str, provider: &str, published: Option<&Value>) -> Value {
    let mut entry = match published {
        Some(Value::Object(fields)) => Value::Object(fields.clone()),
        _ => json!({}),
    };
    entry["id"] = json!(model);
    entry["object"] = json!("model");
    if entry.get("owned_by").and_then(Value::as_str).is_none() {
        entry["owned_by"] = json!(provider);
    }
    entry
}
