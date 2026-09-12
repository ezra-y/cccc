use cccc_core::web_model_connectors::{self, BrowserTargetOwner};
use serde_json::Value;

use crate::AppState;
use crate::api::ApiError;

use super::web_model_connector_activity::{self as activity, Activity};

pub(super) fn target(state: &AppState, group_id: &str, actor_id: &str) -> Result<Value, ApiError> {
    cccc_core::web_model_connectors::browser_target(&state.home, group_id, actor_id)
        .map_err(io_error)
}

pub(super) fn snapshot(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
) -> Result<(Value, BrowserTargetOwner), ApiError> {
    web_model_connectors::browser_target_snapshot(&state.home, group_id, actor_id).map_err(io_error)
}

pub(super) fn update_target(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    owner: &BrowserTargetOwner,
    patch: Value,
) -> Result<bool, ApiError> {
    web_model_connectors::update_browser_target(
        &state.home,
        group_id,
        actor_id,
        owner,
        patch.as_object().expect("browser delivery patch"),
    )
    .map_err(io_error)
}

pub(super) fn record_connector(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    owner: &BrowserTargetOwner,
    status: &str,
    turn_id: &str,
    error: &str,
) -> Result<(), ApiError> {
    web_model_connectors::update_browser_connector(&state.home, group_id, actor_id, owner, |item| {
        activity::apply(
            item,
            &Activity {
                method: "browser/delivery",
                tool_name: "",
                call_status: status,
                wait_status: status,
                turn_id,
                error,
            },
        )
    })
    .map(|_| ())
    .map_err(io_error)
}

fn io_error(error: std::io::Error) -> ApiError {
    ApiError::bad(error.to_string())
}
