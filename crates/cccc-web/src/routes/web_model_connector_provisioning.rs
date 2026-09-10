use axum::Json;
use axum::extract::{Path, State};
use cccc_contracts::ActorRuntime;
use cccc_core::{GroupStore, settings};
use serde_json::{Value, json};

use crate::AppState;
use crate::api::{ApiError, ApiResult, success};

use super::web_model_connector_store as store;
use super::web_model_connectors::required;

pub(super) async fn list(State(state): State<AppState>) -> ApiResult {
    let mut connectors = store::load(&state)?;
    connectors.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    let base_url = connector_base_url(&state)?;
    Ok(success(json!({
        "connectors": connectors.iter().map(|item| public(item, &base_url)).collect::<Vec<_>>()
    })))
}

pub(super) async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> ApiResult {
    let group_id = required(&body, "group_id")?;
    let actor_id = required(&body, "actor_id")?;
    let group = GroupStore::new(state.home.clone())
        .map_err(store::io_error)?
        .load(&group_id)
        .map_err(|_| ApiError::not_found(format!("group not found: {group_id}")))?;
    let actor = group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id)
        .ok_or_else(|| ApiError::not_found(format!("actor not found: {actor_id}")))?;
    if actor.runtime != ActorRuntime::WebModel {
        return Err(ApiError::bad(
            "web-model connectors require an actor with runtime=web_model",
        ));
    }

    let (connector, replaced) = cccc_core::web_model_connectors::create(
        &state.home,
        &group_id,
        &actor_id,
        body.get("provider")
            .and_then(Value::as_str)
            .unwrap_or("chatgpt"),
        body.get("label").and_then(Value::as_str).unwrap_or(""),
    )
    .map_err(store::io_error)?;
    let base_url = connector_base_url(&state)?;
    Ok(success(json!({
        "connector": public(&connector, &base_url),
        "secret": connector["secret"],
        "replaced_connector_ids": replaced
    })))
}

fn connector_base_url(state: &AppState) -> Result<String, ApiError> {
    let settings = settings::load(&state.home).map_err(store::io_error)?;
    let mut base = settings
        .remote_access
        .get("web_public_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_owned();
    if base.ends_with("/ui") {
        base.truncate(base.len() - "/ui".len());
    }
    Ok(base)
}

fn public(item: &Value, base_url: &str) -> Value {
    let mut result = item.as_object().cloned().unwrap_or_default();
    let secret = result
        .remove("secret")
        .and_then(|value| value.as_str().map(str::to_owned));
    result.remove("secret_hash");
    result.insert(
        "session_bound".into(),
        json!(
            item["session_hash"].as_str().is_some_and(|v| !v.is_empty()) && item["revoked"] != true
        ),
    );
    result.insert("session_bound_at".into(), item["session_bound_at"].clone());
    for key in ["session_hash", "previous_session_hash", "binding_code_hash"] {
        result.remove(key);
    }

    let id = item["connector_id"].as_str().unwrap_or("");
    let secret_value = secret.as_deref().unwrap_or_default();
    let connector_path = format!("/mcp/web-model/{id}");
    let connector_url = if base_url.is_empty() {
        connector_path
    } else {
        format!("{base_url}{connector_path}")
    };
    result.insert("secret_available".into(), Value::Bool(secret.is_some()));
    let preview = result
        .get("secret_preview")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    result.insert(
        "secret_preview".into(),
        Value::String(if preview.is_empty() {
            secret.as_deref().map_or(String::new(), |value| {
                if value.len() <= 10 {
                    "****".into()
                } else {
                    format!("{}...{}", &value[..6], &value[value.len() - 4..])
                }
            })
        } else {
            preview
        }),
    );
    result.insert("connector_url".into(), json!(connector_url));
    result.insert(
        "connector_url_path_token".into(),
        json!(format!("{connector_url}/token/{secret_value}")),
    );
    result.insert(
        "connector_url_with_token".into(),
        json!(format!("{connector_url}?token={secret_value}")),
    );
    Value::Object(result)
}

pub(super) async fn revoke(
    State(state): State<AppState>,
    Path(connector_id): Path<String>,
) -> ApiResult {
    if !store::revoke(&state, &connector_id)? {
        return Err(ApiError::not_found("web-model connector not found"));
    }
    Ok(success(json!({"revoked":true,"connector_id":connector_id})))
}

/// Local Web control plane issues a code; the selected Chat consumes it via the gateway.
pub(super) async fn prepare_binding(
    State(state): State<AppState>,
    Path(connector_id): Path<String>,
) -> ApiResult {
    let connector = store::find_authorized(&state, &connector_id, None)?;
    let mut binding =
        cccc_core::web_model_connectors::prepare_binding(&state.home, &connector_id, 600)
            .map_err(store::io_error)?;
    binding["group_id"] = connector["group_id"].clone();
    binding["actor_id"] = connector["actor_id"].clone();
    binding["session_bound"] = json!(
        connector["session_hash"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    Ok(success(binding))
}

#[cfg(test)]
mod tests {
    #[test]
    fn public_connector_contains_binding_status_without_private_binding_material() {
        let item = serde_json::json!({"connector_id":"wmc_test","group_id":"g_test","actor_id":"lead",
            "secret":"private-secret","secret_hash":"private-hash","session_hash":"private-session",
            "binding_code_hash":"private-code","previous_session_hash":"previous-private-session",
            "session_bound_at":"2026-09-05T00:00:00Z"});
        let result = super::public(&item, "");
        assert_eq!(result["session_bound"], true);
        for name in [
            "secret",
            "secret_hash",
            "session_hash",
            "binding_code_hash",
            "previous_session_hash",
        ] {
            assert!(result.get(name).is_none(), "private field {name}");
        }
    }
}
