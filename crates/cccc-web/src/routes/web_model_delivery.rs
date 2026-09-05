use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::api::ApiError;
use crate::browser_surface::{
    BOUND_CONVERSATION_ERROR_MARKER, PromptSubmissionOutcome, conversation_url_for_target,
    stored_verified_submission_evidence,
};

use super::web_model_browser::{key, surface_key};
use super::web_model_delivery_completion::{
    args, call as daemon_call, complete_args, reconcile, record_delivery,
};
use super::web_model_delivery_state::{record_connector, target as load_target, update_target};

static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static WORKERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
pub(super) const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const DEFERRED_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(3);
const DEFERRED_MAX_AUTOMATIC_RETRIES: u32 = 3;

fn deferred_retry_delay(retries: u32) -> Option<std::time::Duration> {
    (retries < DEFERRED_MAX_AUTOMATIC_RETRIES).then(|| DEFERRED_RETRY_BASE * (1_u32 << retries))
}

const BOOTSTRAP_SEED_VERSION: &str = "web-model-bootstrap-normal-system-prompt-v2";
const COMPATIBILITY_IMAGE_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAKUlEQVR42u3OIQEAAAACIP+f1hkWWEB6FgEBAQEBAQEBAQEBAQEBgXdgl/rw4tnPBf0AAAAASUVORK5CYII=";
const COMPATIBILITY_IMAGE_NOTE: &str = "[CCCC] Compatibility attachment: the blank image is transport-only and carries no task context.";
const WEB_TRANSPORT_NOTE: &str = "[CCCC] Web transport:\n\
- This browser conversation is the web surface for the actor above.\n\
- Browser-injected messages are already delivered in chat; do not call cccc_runtime_wait_next_turn for them.\n\
- Use CCCC MCP tools for visible replies, handoffs, local workspace work, validation, and evidence.\n\
- For non-trivial local development work, default to cccc_code_exec so repo reads, patches, tests, diffs, and reports stay in one focused Codex-style loop; use direct tools only for simple one-step actions.\n\
- If CCCC MCP tools are not visible in the selected web model, you do not have CCCC local access in this chat; tell the user to switch to a supported session that can see the CCCC connector.\n\
- Text typed only in this web chat is not delivered to CCCC users or peers.";

struct BootstrapSeed {
    text: String,
    digest: String,
}

struct DeliveryAttempt<'a> {
    turn_id: &'a str,
    event_ids: Value,
    delivery_id: &'a str,
}

pub(super) enum DeliveryOutcome {
    Submitted,
    Idle,
    Deferred(String),
    Ambiguous,
    Stopped,
}

pub(super) async fn ensure_worker(state: AppState, group_id: String, actor_id: String) {
    spawn_worker(state, group_id, actor_id);
}

fn spawn_worker(state: AppState, group_id: String, actor_id: String) {
    let session_key = key(&group_id, &actor_id);
    let Some(worker) = SessionGuard::acquire(&WORKERS, session_key.clone()) else {
        return;
    };
    tokio::spawn(async move {
        // Keep the worker guard in this scope so it is always released before the fresh-turn
        // check below. An event arriving during the final deferred attempt cannot acquire the
        // guard, so that check is responsible for recovering its wake-up.
        let exhausted_turn_id = {
            let _worker = worker;
            let mut exhausted_turn_id = None;
            let mut retry_seconds = 1_u64;
            let mut deferred_turn_id = String::new();
            let mut deferred_retries = 0_u32;
            let mut shutdown = state.shutdown.subscribe();
            loop {
                let surface = state.browser_surfaces.info(surface_key()).await;
                if !surface["active"].as_bool().unwrap_or(false) {
                    break;
                }
                let delay = match deliver_pending(&state, &group_id, &actor_id).await {
                    Ok(DeliveryOutcome::Submitted) => {
                        retry_seconds = 1;
                        deferred_turn_id.clear();
                        deferred_retries = 0;
                        std::time::Duration::from_millis(10)
                    }
                    Ok(DeliveryOutcome::Deferred(turn_id)) => {
                        retry_seconds = 1;
                        if deferred_turn_id != turn_id {
                            deferred_turn_id = turn_id;
                            deferred_retries = 0;
                        }
                        let Some(delay) = deferred_retry_delay(deferred_retries) else {
                            tracing::info!(
                                group_id,
                                actor_id,
                                turn_id = deferred_turn_id,
                                "Web-model browser deferred retry budget exhausted"
                            );
                            exhausted_turn_id = Some(deferred_turn_id.clone());
                            break;
                        };
                        deferred_retries += 1;
                        delay
                    }
                    Ok(DeliveryOutcome::Idle | DeliveryOutcome::Ambiguous) => {
                        retry_seconds = 1;
                        deferred_turn_id.clear();
                        deferred_retries = 0;
                        IDLE_POLL_INTERVAL
                    }
                    Ok(DeliveryOutcome::Stopped) => break,
                    Err(error) => {
                        tracing::warn!(
                            group_id,
                            actor_id,
                            %error,
                            "Web-model browser delivery failed; retrying"
                        );
                        retry_seconds = (retry_seconds * 2).min(30);
                        std::time::Duration::from_secs(retry_seconds)
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = shutdown.recv() => break,
                }
            }
            exhausted_turn_id
        };

        let Some(exhausted_turn_id) = exhausted_turn_id else {
            return;
        };
        if let Ok(target) = load_target(&state, &group_id, &actor_id) {
            let message =
                "browser model remained unavailable after the bounded automatic retry budget";
            let _ = update_target(
                &state,
                &group_id,
                &actor_id,
                json!({"last_delivery_status":"failed","last_error":message}),
            );
            if let (Some(delivery_id), Some(event_ids)) = (
                target["last_delivery_id"].as_str(),
                target["last_delivery_event_ids"].as_array(),
            ) {
                let _ = record_delivery(
                    &state,
                    &group_id,
                    &actor_id,
                    &exhausted_turn_id,
                    Value::Array(event_ids.clone()),
                    delivery_id,
                    "failed",
                    message,
                    json!({"target_url":target["url"]}),
                )
                .await;
            }
        }
        match fresh_turn_after_exhaustion(&state, &group_id, &actor_id, &exhausted_turn_id).await {
            Ok(Some(fresh_turn_id)) => {
                tracing::debug!(
                    group_id,
                    actor_id,
                    exhausted_turn_id,
                    fresh_turn_id,
                    "Rescheduling Web-model browser delivery for fresh direct work"
                );
                spawn_worker(state, group_id, actor_id);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    group_id,
                    actor_id,
                    %error,
                    "Web-model browser fresh unread check failed"
                );
            }
        }
    });
}

async fn fresh_turn_after_exhaustion(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    exhausted_turn_id: &str,
) -> Result<Option<String>, ApiError> {
    if !super::web_model_supervisor::actor_delivery_enabled(state, group_id, actor_id) {
        return Ok(None);
    }
    let wait = daemon_call(
        state,
        "runtime_wait_next_turn",
        browser_wait_args(group_id, actor_id),
    )
    .await?;
    Ok(replacement_turn_id(exhausted_turn_id, &wait))
}

fn replacement_turn_id(exhausted_turn_id: &str, wait: &Value) -> Option<String> {
    if wait["status"] != "work_available" {
        return None;
    }
    wait["turn"]["turn_id"]
        .as_str()
        .map(str::trim)
        .filter(|turn_id| !turn_id.is_empty() && *turn_id != exhausted_turn_id)
        .map(str::to_owned)
}

pub(super) async fn deliver_pending(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
) -> Result<DeliveryOutcome, ApiError> {
    let session_key = key(group_id, actor_id);
    let Some(_delivery) = SessionGuard::acquire(&IN_FLIGHT, session_key.clone()) else {
        return Ok(DeliveryOutcome::Idle);
    };
    let _operation = state.browser_surfaces.web_model_operation.lock().await;
    if super::web_model_browser::another_chat_is_pending(state, group_id, actor_id)? {
        return Ok(DeliveryOutcome::Idle);
    }
    deliver_once(state, group_id, actor_id, surface_key()).await
}

struct SessionGuard {
    sessions: &'static Mutex<HashSet<String>>,
    key: String,
}

impl SessionGuard {
    fn acquire(storage: &'static OnceLock<Mutex<HashSet<String>>>, key: String) -> Option<Self> {
        let sessions = storage.get_or_init(|| Mutex::new(HashSet::new()));
        let inserted = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone());
        // Construct an owning guard only after acquisition succeeds. Eager
        // then_some drops the rejected guard and releases the current owner.
        inserted.then(|| Self { sessions, key })
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

async fn deliver_once(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    session_key: &str,
) -> Result<DeliveryOutcome, ApiError> {
    if !super::web_model_supervisor::actor_delivery_enabled(state, group_id, actor_id) {
        return Ok(DeliveryOutcome::Stopped);
    }
    let surface = state.browser_surfaces.info(session_key).await;
    if !surface["active"].as_bool().unwrap_or(false) {
        return Ok(DeliveryOutcome::Idle);
    }
    let target = load_target(state, group_id, actor_id)?;
    let target_url = target["url"].as_str().unwrap_or("");
    if target["last_delivery_status"] == "submitting" {
        let message = "browser delivery was interrupted after its at-most-once dispatch fence; the message will not be redelivered automatically";
        let evidence = json!({
            "submitted":false,
            "submission_evidence":"interrupted_dispatch",
            "error":message
        });
        return complete_ambiguous_attempt(
            state,
            group_id,
            actor_id,
            DeliveryAttempt {
                turn_id: required(&target, "last_delivery_turn_id")?,
                event_ids: target["last_delivery_event_ids"].clone(),
                delivery_id: required(&target, "last_delivery_id")?,
            },
            evidence,
            message,
        )
        .await;
    }
    if target["last_delivery_status"] == "legacy_recovery_submitting" {
        let message = "legacy browser recovery was interrupted after dispatch began; the committed message will not be submitted again";
        update_target(
            state,
            group_id,
            actor_id,
            json!({
                "last_delivery_status":"submission_ambiguous",
                "last_delivery_at":cccc_contracts::utc_now(),
                "last_submission_evidence":{
                    "submitted":false,
                    "submission_evidence":"interrupted_legacy_dispatch",
                    "error":message
                },
                "last_error":message
            }),
        )?;
        record_connector(
            state,
            group_id,
            actor_id,
            "ambiguous",
            target["last_delivery_turn_id"].as_str().unwrap_or(""),
            message,
        )?;
        if target["kind"] == "new_chat" {
            return resolve_pending_new_chat(
                state,
                group_id,
                actor_id,
                session_key,
                &load_target(state, group_id, actor_id)?,
            )
            .await;
        }
        return Ok(DeliveryOutcome::Ambiguous);
    }
    if target_url.is_empty() && target["kind"] != "new_chat" {
        return Ok(DeliveryOutcome::Idle);
    }
    if target["last_delivery_status"] == "submission_ambiguous" {
        if recover_verified_ambiguous_submission(state, group_id, actor_id, &target).await? {
            return Ok(DeliveryOutcome::Submitted);
        }
        // The attempted turn was already committed to preserve at-most-once delivery. A known
        // conversation target can therefore continue with later turns without retrying it. A new
        // chat must remain fenced until its conversation URL can be recovered.
        if target["kind"] == "new_chat" {
            return resolve_pending_new_chat(state, group_id, actor_id, session_key, &target).await;
        }
    }
    if is_legacy_pending_delivery(&target) {
        if state
            .browser_surfaces
            .wait_for_conversation_url(session_key, target_url, std::time::Duration::ZERO)
            .await
            .map_err(|error| {
                ApiError::unavailable("web_model_conversation_bind_failed", error.to_string())
            })?
            .is_some()
        {
            return resolve_pending_new_chat(state, group_id, actor_id, session_key, &target).await;
        }
        return recover_legacy_pending_delivery(state, group_id, actor_id, session_key, &target)
            .await;
    }
    if matches!(
        target["last_delivery_status"].as_str(),
        Some(
            "ambiguous"
                | "completion_ambiguous"
                | "submission_ambiguous_completion_pending"
                | "completion_conflict"
        )
    ) {
        if !reconcile(state, group_id, actor_id, &target).await? {
            return Ok(DeliveryOutcome::Ambiguous);
        }
        let reconciled = load_target(state, group_id, actor_id)?;
        if reconciled["last_delivery_status"] == "submission_ambiguous" {
            if reconciled["kind"] == "new_chat" {
                return resolve_pending_new_chat(
                    state,
                    group_id,
                    actor_id,
                    session_key,
                    &reconciled,
                )
                .await;
            }
        } else {
            if reconciled["kind"] == "new_chat" {
                return resolve_pending_new_chat(
                    state,
                    group_id,
                    actor_id,
                    session_key,
                    &reconciled,
                )
                .await;
            }
            return Ok(DeliveryOutcome::Submitted);
        }
    }
    if target["kind"] == "new_chat"
        && matches!(
            target["last_delivery_status"].as_str(),
            Some("submitted" | "pending_new_chat_bind")
        )
    {
        return resolve_pending_new_chat(state, group_id, actor_id, session_key, &target).await;
    }
    let wait = daemon_call(
        state,
        "runtime_wait_next_turn",
        browser_wait_args(group_id, actor_id),
    )
    .await?;
    if wait["status"] != "work_available" {
        return Ok(DeliveryOutcome::Idle);
    }
    let turn = &wait["turn"];
    let turn_id = required(turn, "turn_id")?;
    let delivery_id = browser_delivery_id(actor_id, turn_id);
    let event_label = turn["event_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let (browser_prompt, bootstrap_seed) = build_browser_prompt(
        turn,
        &target,
        target_url,
        actor_id,
        &delivery_id,
        &event_label,
    )?;
    let attachment = compatibility_attachment(state, turn, &delivery_id)?;
    update_target(
        state,
        group_id,
        actor_id,
        json!({"last_delivery_id":delivery_id,"last_delivery_turn_id":turn_id,"last_delivery_event_ids":turn["event_ids"],"last_delivery_status":"submitting","last_delivery_started_at":cccc_contracts::utc_now(),"last_error":""}),
    )?;
    record_delivery(
        state,
        group_id,
        actor_id,
        turn_id,
        turn["event_ids"].clone(),
        &delivery_id,
        "submitting",
        "",
        json!({"target_url":target_url,"auto_bind_new_chat":target["kind"] == "new_chat"}),
    )
    .await?;
    let submitted = state
        .browser_surfaces
        .submit_prompt_with_attachment(
            session_key,
            target_url,
            &browser_prompt,
            attachment.as_deref(),
            &delivery_id,
        )
        .await;
    let browser = match submitted {
        Ok(PromptSubmissionOutcome::Verified(browser)) => browser,
        Ok(PromptSubmissionOutcome::Deferred(browser)) => {
            let busy = matches!(
                browser["submission_evidence"].as_str(),
                Some("not_sent_chat_busy" | "not_sent_composer_occupied")
            );
            let message = "browser model is not ready for a safe prompt submission";
            update_target(
                state,
                group_id,
                actor_id,
                json!({"last_delivery_status":"deferred","last_submission_evidence":browser,"last_error":message}),
            )?;
            record_delivery(
                state,
                group_id,
                actor_id,
                turn_id,
                turn["event_ids"].clone(),
                &delivery_id,
                "failed",
                message,
                json!({"target_url":target_url}),
            )
            .await?;
            record_connector(state, group_id, actor_id, "deferred", turn_id, message)?;
            // A running Chat or an unsent human draft is not a failed connection.
            // Reuse the existing idle cadence rather than exhausting technical retries.
            return Ok(if busy {
                DeliveryOutcome::Idle
            } else {
                DeliveryOutcome::Deferred(turn_id.to_owned())
            });
        }
        Ok(PromptSubmissionOutcome::Ambiguous(browser)) => {
            let message = "browser submission was attempted but could not be verified; this message will not be redelivered automatically";
            return complete_ambiguous_attempt(
                state,
                group_id,
                actor_id,
                DeliveryAttempt {
                    turn_id,
                    event_ids: turn["event_ids"].clone(),
                    delivery_id: &delivery_id,
                },
                browser,
                message,
            )
            .await;
        }
        Err(error) if error.to_string().contains(BOUND_CONVERSATION_ERROR_MARKER) => {
            let message = error.to_string();
            update_target(
                state,
                group_id,
                actor_id,
                json!({
                    "last_delivery_status":"failed",
                    "last_submission_evidence":{
                        "submitted":false,
                        "submission_evidence":"bound_conversation_unavailable",
                        "error":message.as_str()
                    },
                    "last_error":message.as_str()
                }),
            )?;
            record_connector(state, group_id, actor_id, "failed", turn_id, &message)?;
            record_delivery(
                state,
                group_id,
                actor_id,
                turn_id,
                turn["event_ids"].clone(),
                &delivery_id,
                "failed",
                &message,
                json!({"target_url":target_url}),
            )
            .await?;
            return Ok(DeliveryOutcome::Stopped);
        }
        Err(error) => {
            let message = format!(
                "browser delivery failed after its at-most-once dispatch fence: {error}; this message will not be redelivered automatically"
            );
            let evidence = json!({
                "submitted":false,
                "submission_evidence":"browser_error_after_dispatch_fence",
                "error":error.to_string()
            });
            return complete_ambiguous_attempt(
                state,
                group_id,
                actor_id,
                DeliveryAttempt {
                    turn_id,
                    event_ids: turn["event_ids"].clone(),
                    delivery_id: &delivery_id,
                },
                evidence,
                &message,
            )
            .await;
        }
    };
    update_target(
        state,
        group_id,
        actor_id,
        completion_pending_patch(
            turn_id,
            turn["event_ids"].clone(),
            browser.clone(),
            bootstrap_seed.as_ref(),
            target_url,
        ),
    )?;
    let submission_evidence = browser["submission_evidence"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    // A verified browser handoff is the terminal delivery fact. Persist it
    // before completing the structured turn so the daemon can validate that
    // every source event actually crossed the runtime boundary.
    record_delivery(
        state,
        group_id,
        actor_id,
        turn_id,
        turn["event_ids"].clone(),
        &delivery_id,
        "submitted",
        &submission_evidence,
        json!({
            "target_url":target_url,
            "auto_bind_new_chat":target["kind"] == "new_chat"
        }),
    )
    .await?;
    let complete = complete_args(
        group_id,
        actor_id,
        turn_id,
        turn["event_ids"].clone(),
        &delivery_id,
    );
    if let Err(error) = daemon_call(state, "runtime_complete_turn", complete).await {
        update_target(
            state,
            group_id,
            actor_id,
            json!({"last_delivery_status":"completion_ambiguous","last_delivery_turn_id":turn_id,"last_delivery_event_ids":turn["event_ids"],"last_delivery_reconcile_attempts":0,"last_submission_evidence":browser,"last_error":error.to_string()}),
        )?;
        tracing::warn!(
            group_id,
            actor_id,
            turn_id,
            %error,
            "Web-model browser submission is ambiguous; automatic redelivery is paused"
        );
        return Ok(DeliveryOutcome::Ambiguous);
    }
    let mut pending_new_chat_bind = target["kind"] == "new_chat";
    let mut bind_error = String::new();
    let mut bound_conversation_url = String::new();
    if pending_new_chat_bind {
        match state
            .browser_surfaces
            .wait_for_conversation_url(session_key, target_url, std::time::Duration::from_secs(15))
            .await
        {
            Ok(Some(conversation_url)) => {
                if let Err(error) =
                    bind_new_chat_target(state, group_id, actor_id, &conversation_url)
                {
                    bind_error = error.to_string();
                } else {
                    bound_conversation_url = conversation_url;
                    pending_new_chat_bind = false;
                }
            }
            Ok(None) => {}
            Err(error) => bind_error = error.to_string(),
        }
    }
    let final_status = if pending_new_chat_bind {
        "pending_new_chat_bind"
    } else {
        "submitted"
    };
    let final_error = if !bind_error.is_empty() {
        bind_error.as_str()
    } else if pending_new_chat_bind {
        "conversation_url_pending"
    } else {
        ""
    };
    let now = cccc_contracts::utc_now();
    let mut final_patch = json!({
        "last_delivery_status":final_status,
        "last_delivery_at":now.clone(),
        "last_error":final_error,
        "last_submission_evidence":browser
    });
    if pending_new_chat_bind {
        final_patch.as_object_mut().expect("delivery patch").extend(
            json!({
                "state":"new_chat_submitted",
                "submitted_at":now,
                "delivery_id":delivery_id,
                "next_delivery":"wait_for_new_chat_bind"
            })
            .as_object()
            .cloned()
            .expect("pending new chat patch"),
        );
    }
    update_target(state, group_id, actor_id, final_patch)?;
    // Keep the existing connector-facing status coherent with the target
    // before the best-effort ledger receipt performs an async daemon call.
    // Otherwise observers can see a submitted target while the connector
    // still exposes the preceding MCP probe status.
    record_connector(state, group_id, actor_id, "submitted", turn_id, "")?;
    if !bound_conversation_url.is_empty() {
        record_delivery(
            state,
            group_id,
            actor_id,
            turn_id,
            turn["event_ids"].clone(),
            &delivery_id,
            "bound",
            &submission_evidence,
            json!({
                "target_url":target_url,
                "bound_conversation_url":bound_conversation_url,
                "pending_conversation_url":false,
                "auto_bind_new_chat":true
            }),
        )
        .await?;
    }
    if pending_new_chat_bind {
        record_delivery(
            state,
            group_id,
            actor_id,
            turn_id,
            turn["event_ids"].clone(),
            &delivery_id,
            "pending",
            final_error,
            json!({
                "target_url":target_url,
                "pending_conversation_url":true,
                "auto_bind_new_chat":true
            }),
        )
        .await?;
    }
    Ok(DeliveryOutcome::Submitted)
}

async fn complete_ambiguous_attempt(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    attempt: DeliveryAttempt<'_>,
    browser: Value,
    message: &str,
) -> Result<DeliveryOutcome, ApiError> {
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "last_delivery_status":"submission_ambiguous_completion_pending",
            "last_delivery_turn_id":attempt.turn_id,
            "last_delivery_event_ids":attempt.event_ids.clone(),
            "last_delivery_reconcile_attempts":0,
            "last_delivery_at":cccc_contracts::utc_now(),
            "last_submission_evidence":browser,
            "last_error":message
        }),
    )?;
    let complete = complete_args(
        group_id,
        actor_id,
        attempt.turn_id,
        attempt.event_ids.clone(),
        attempt.delivery_id,
    );
    record_delivery(
        state,
        group_id,
        actor_id,
        attempt.turn_id,
        attempt.event_ids.clone(),
        attempt.delivery_id,
        "ambiguous",
        message,
        json!({}),
    )
    .await?;
    let completion = daemon_call(state, "runtime_complete_turn", complete).await;
    let completion_status = if completion.is_ok() {
        "submission_ambiguous"
    } else {
        "submission_ambiguous_completion_pending"
    };
    let completion_error = completion
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "last_delivery_status":completion_status,
            "last_error":if completion_error.is_empty() {message} else {&completion_error}
        }),
    )?;
    if completion.is_ok() {
        record_connector(
            state,
            group_id,
            actor_id,
            "ambiguous",
            attempt.turn_id,
            message,
        )?;
    }
    tracing::warn!(
        group_id,
        actor_id,
        turn_id = attempt.turn_id,
        completion_recorded = completion.is_ok(),
        "Web-model browser submission could not be verified; the attempted message will not be redelivered automatically"
    );
    Ok(DeliveryOutcome::Ambiguous)
}

async fn recover_verified_ambiguous_submission(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    target: &Value,
) -> Result<bool, ApiError> {
    let submission = &target["last_submission_evidence"];
    let Some(submission_evidence) = stored_verified_submission_evidence(submission) else {
        return Ok(false);
    };
    let turn_id = required(target, "last_delivery_turn_id")?;
    let delivery_id = required(target, "last_delivery_id")?;
    let mut recover_args = args(group_id, actor_id);
    recover_args.insert(
        "event_ids".into(),
        target["last_delivery_event_ids"].clone(),
    );
    let recovered = daemon_call(state, "web_model_runtime_recover_turn", recover_args).await?;
    let turn = &recovered["turn"];
    let target_url = target["url"].as_str().unwrap_or("");
    let observed_url = submission["observed"]["url"].as_str().unwrap_or("");
    let conversation_url = conversation_url_for_target(target_url, observed_url);
    let event_label = target["last_delivery_event_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let (_, bootstrap_seed) = build_browser_prompt(
        turn,
        target,
        target_url,
        actor_id,
        delivery_id,
        &event_label,
    )?;
    if let Some(seed) = &bootstrap_seed {
        mark_bootstrap_seed_delivered(
            state,
            group_id,
            actor_id,
            conversation_url.as_deref().unwrap_or(target_url),
            seed,
        )?;
    }
    if target["kind"] == "new_chat"
        && let Some(conversation_url) = &conversation_url
    {
        bind_new_chat_target(state, group_id, actor_id, conversation_url)?;
    }
    let pending_new_chat_bind = target["kind"] == "new_chat" && conversation_url.is_none();
    let mut recovered_submission = submission.clone();
    if let Some(object) = recovered_submission.as_object_mut() {
        object.insert("submitted".into(), json!(true));
        object.insert("submission_evidence".into(), json!(submission_evidence));
        object.insert("recovered_from".into(), json!("submission_ambiguous"));
    }
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "last_delivery_status":if pending_new_chat_bind {"pending_new_chat_bind"} else {"submitted"},
            "last_delivery_at":cccc_contracts::utc_now(),
            "last_submission_evidence":recovered_submission,
            "last_error":if pending_new_chat_bind {"conversation_url_pending"} else {""}
        }),
    )?;
    record_connector(state, group_id, actor_id, "submitted", turn_id, "")?;
    tracing::info!(
        group_id,
        actor_id,
        turn_id,
        submission_evidence,
        conversation_bound = conversation_url.is_some(),
        "Recovered a browser submission from persisted direct evidence"
    );
    Ok(true)
}

fn is_legacy_pending_delivery(target: &Value) -> bool {
    target["kind"] == "new_chat"
        && matches!(
            target["last_delivery_status"].as_str(),
            Some("submitted" | "pending_new_chat_bind")
        )
        && target["last_delivery_id"]
            .as_str()
            .is_some_and(|delivery_id| delivery_id.starts_with("wmd_"))
        && target["last_submission_evidence"]["submission_evidence"].as_str()
            != Some("message_echo")
}

async fn recover_legacy_pending_delivery(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    session_key: &str,
    target: &Value,
) -> Result<DeliveryOutcome, ApiError> {
    let event_ids = target["last_delivery_event_ids"].clone();
    let mut recover_args = args(group_id, actor_id);
    recover_args.insert("event_ids".into(), event_ids.clone());
    let recovered = daemon_call(state, "web_model_runtime_recover_turn", recover_args).await?;
    let turn = &recovered["turn"];
    let old_prompt = legacy_wmd_staged_prompt(turn)?;
    let target_url = required(target, "url")?;
    let inspection = state
        .browser_surfaces
        .inspect_staged_prompt(session_key, target_url, &old_prompt)
        .await
        .map_err(|error| {
            ApiError::unavailable("web_model_legacy_inspection_failed", error.to_string())
        })?;
    if !inspection["recoverable"].as_bool().unwrap_or(false) {
        let message = "legacy browser submission cannot be verified automatically; the draft or page state no longer matches the committed turn";
        update_target(
            state,
            group_id,
            actor_id,
            json!({
                "last_delivery_status":"legacy_submission_unverified",
                "last_submission_evidence":inspection,
                "last_error":message
            }),
        )?;
        record_connector(
            state,
            group_id,
            actor_id,
            "ambiguous",
            target["last_delivery_turn_id"].as_str().unwrap_or(""),
            message,
        )?;
        return Ok(DeliveryOutcome::Ambiguous);
    }

    let turn_id = required(turn, "turn_id")?;
    let delivery_id = browser_delivery_id(actor_id, turn_id);
    let event_label = turn["event_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let (browser_prompt, bootstrap_seed) = build_browser_prompt(
        turn,
        target,
        target_url,
        actor_id,
        &delivery_id,
        &event_label,
    )?;
    let attachment = compatibility_attachment(state, turn, &delivery_id)?;
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "last_delivery_id":delivery_id,
            "last_delivery_turn_id":turn_id,
            "last_delivery_event_ids":event_ids,
            "last_delivery_status":"legacy_recovery_submitting",
            "last_delivery_started_at":cccc_contracts::utc_now(),
            "last_error":""
        }),
    )?;
    let browser = match state
        .browser_surfaces
        .submit_prompt_with_attachment(
            session_key,
            target_url,
            &browser_prompt,
            attachment.as_deref(),
            &delivery_id,
        )
        .await
    {
        Ok(PromptSubmissionOutcome::Verified(browser)) => browser,
        Ok(PromptSubmissionOutcome::Deferred(browser)) => {
            let message = "legacy delivery was safely restaged, but ChatGPT did not expose an enabled Send control";
            update_target(
                state,
                group_id,
                actor_id,
                json!({
                    "last_delivery_status":"legacy_submission_unverified",
                    "last_submission_evidence":browser,
                    "last_error":message
                }),
            )?;
            return Ok(DeliveryOutcome::Deferred(turn_id.to_owned()));
        }
        Ok(PromptSubmissionOutcome::Ambiguous(browser)) => {
            let message = "legacy recovery attempted submission but could not verify whether ChatGPT accepted it; automatic redelivery is paused";
            update_target(
                state,
                group_id,
                actor_id,
                json!({
                    "last_delivery_status":"submission_ambiguous",
                    "last_submission_evidence":browser,
                    "last_error":message,
                    "last_delivery_at":cccc_contracts::utc_now()
                }),
            )?;
            record_connector(state, group_id, actor_id, "ambiguous", turn_id, message)?;
            return Ok(DeliveryOutcome::Ambiguous);
        }
        Err(error) => {
            update_target(
                state,
                group_id,
                actor_id,
                json!({"last_delivery_status":"failed","last_error":error.to_string()}),
            )?;
            return Err(ApiError::unavailable(
                "web_model_legacy_recovery_failed",
                error.to_string(),
            ));
        }
    };
    if let Some(seed) = &bootstrap_seed {
        mark_bootstrap_seed_delivered(state, group_id, actor_id, target_url, seed)?;
    }
    let conversation_url = state
        .browser_surfaces
        .wait_for_conversation_url(session_key, target_url, std::time::Duration::from_secs(15))
        .await
        .map_err(|error| {
            ApiError::unavailable("web_model_conversation_bind_failed", error.to_string())
        })?;
    let pending = conversation_url.is_none();
    if let Some(conversation_url) = conversation_url {
        bind_new_chat_target(state, group_id, actor_id, &conversation_url)?;
    }
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "last_delivery_status":if pending {"pending_new_chat_bind"} else {"submitted"},
            "last_delivery_at":cccc_contracts::utc_now(),
            "last_submission_evidence":browser,
            "last_error":if pending {"conversation_url_pending"} else {""}
        }),
    )?;
    record_connector(state, group_id, actor_id, "submitted", turn_id, "")?;
    Ok(DeliveryOutcome::Submitted)
}

fn legacy_wmd_staged_prompt(turn: &Value) -> Result<String, ApiError> {
    let actor_id = required(turn, "actor_id")?;
    let messages = turn["messages"]
        .as_array()
        .ok_or_else(|| ApiError::bad("recovered runtime turn missing messages"))?;
    let mut output = messages
        .iter()
        .map(|event| {
            let by = event["by"].as_str().unwrap_or_default();
            let text = event["data"]["text"].as_str().unwrap_or_default();
            format!("[{by} -> {actor_id}] {text}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if output.chars().count() > 24_000 {
        output = output.chars().take(23_920).collect();
        output.push_str("\n\n[cccc] coalesced turn text truncated");
    }
    Ok(output)
}

fn browser_delivery_id(actor_id: &str, turn_id: &str) -> String {
    let turn_key = turn_id.rsplit(':').next().unwrap_or(turn_id);
    format!("webdelivery:{actor_id}:{turn_key}")
}

fn browser_wait_args(group_id: &str, actor_id: &str) -> serde_json::Map<String, Value> {
    let mut request = args(group_id, actor_id);
    request.insert("transport".into(), json!("web_model_browser"));
    request
}

fn build_browser_prompt(
    turn: &Value,
    target: &Value,
    target_url: &str,
    actor_id: &str,
    delivery_id: &str,
    event_label: &str,
) -> Result<(String, Option<BootstrapSeed>), ApiError> {
    let prompt = required(turn, "coalesced_text")?;
    let system_prompt = required(turn, "system_prompt")?;
    let seed_text = format!(
        "[CCCC] Session bootstrap for this browser chat:\n\n{system_prompt}\n\n{WEB_TRANSPORT_NOTE}"
    );
    let digest = bootstrap_seed_digest(&seed_text);
    let seed_required = target["bootstrap_seed_delivered_at"]
        .as_str()
        .is_none_or(str::is_empty)
        || target["bootstrap_seed_version"].as_str() != Some(BOOTSTRAP_SEED_VERSION)
        || target["bootstrap_seed_digest"].as_str() != Some(digest.as_str())
        || target["bootstrap_seed_conversation_url"].as_str() != Some(target_url);
    let seed = seed_required.then_some(BootstrapSeed {
        text: seed_text,
        digest,
    });
    let setup = seed
        .as_ref()
        .map(|seed| format!("{}\n\n", seed.text))
        .unwrap_or_default();
    let compatibility_note = if turn["delivery"]["web_model_mode"] == "image_compat" {
        format!("{COMPATIBILITY_IMAGE_NOTE}\n")
    } else {
        String::new()
    };
    Ok((
        format!(
            "{setup}[cccc] Browser batch {delivery_id} events={event_label} actor={actor_id}\n{compatibility_note}{prompt}"
        ),
        seed,
    ))
}

fn compatibility_attachment(
    state: &AppState,
    turn: &Value,
    delivery_id: &str,
) -> Result<Option<PathBuf>, ApiError> {
    if turn["delivery"]["web_model_mode"] != "image_compat" {
        return Ok(None);
    }
    let (filename, bytes) = compatibility_image_for_delivery(delivery_id)?;
    let directory = state.home.root().join("cache/web-model");
    std::fs::create_dir_all(&directory).map_err(|error| {
        ApiError::unavailable("web_model_attachment_cache_failed", error.to_string())
    })?;
    let path = directory.join(filename);
    let current = std::fs::read(&path).ok();
    if current.as_deref() != Some(bytes.as_slice()) {
        cccc_core::fs::atomic_write(&path, &bytes).map_err(|error| {
            ApiError::unavailable("web_model_attachment_cache_failed", error.to_string())
        })?;
    }
    Ok(Some(path))
}

fn compatibility_image_for_delivery(delivery_id: &str) -> Result<(String, Vec<u8>), ApiError> {
    let delivery_id = delivery_id.trim();
    if delivery_id.is_empty() {
        return Err(ApiError::bad("compatibility image delivery_id is required"));
    }
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(COMPATIBILITY_IMAGE_B64)
        .map_err(|error| ApiError::bad(format!("decode compatibility image: {error}")))?;
    let digest = format!("{:x}", Sha256::digest(delivery_id.as_bytes()));
    let iend_offset = bytes
        .len()
        .checked_sub(12)
        .filter(|offset| bytes.get(*offset + 4..*offset + 8) == Some(b"IEND"))
        .ok_or_else(|| ApiError::bad("compatibility image is missing its terminal PNG chunk"))?;
    let mut marker = b"CCCC-Delivery\0".to_vec();
    marker.extend_from_slice(digest.as_bytes());
    let marker_len = u32::try_from(marker.len())
        .map_err(|_| ApiError::bad("compatibility image marker is too large"))?;
    let mut chunk = Vec::with_capacity(marker.len() + 12);
    chunk.extend_from_slice(&marker_len.to_be_bytes());
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(&marker);
    let mut checksum = crc32fast::Hasher::new();
    checksum.update(b"tEXt");
    checksum.update(&marker);
    chunk.extend_from_slice(&checksum.finalize().to_be_bytes());
    bytes.splice(iend_offset..iend_offset, chunk);
    Ok((format!("cccc-mcp-compat-{}.png", &digest[..16]), bytes))
}

fn bootstrap_seed_digest(seed: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(seed.as_bytes()));
    digest[..20].to_owned()
}

fn completion_pending_patch(
    turn_id: &str,
    event_ids: Value,
    browser: Value,
    bootstrap_seed: Option<&BootstrapSeed>,
    target_url: &str,
) -> Value {
    let mut patch = json!({
        "last_delivery_status":"completion_ambiguous",
        "last_delivery_turn_id":turn_id,
        "last_delivery_event_ids":event_ids,
        "last_delivery_reconcile_attempts":0,
        "last_delivery_at":cccc_contracts::utc_now(),
        "last_submission_evidence":browser,
        "last_error":"delivery_completion_pending"
    });
    if let Some(seed) = bootstrap_seed {
        patch["bootstrap_seed_delivered_at"] = json!(cccc_contracts::utc_now());
        patch["bootstrap_seed_version"] = json!(BOOTSTRAP_SEED_VERSION);
        patch["bootstrap_seed_digest"] = json!(seed.digest);
        patch["bootstrap_seed_conversation_url"] = json!(target_url);
    }
    patch
}

fn mark_bootstrap_seed_delivered(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    target_url: &str,
    seed: &BootstrapSeed,
) -> Result<(), ApiError> {
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "bootstrap_seed_delivered_at":cccc_contracts::utc_now(),
            "bootstrap_seed_version":BOOTSTRAP_SEED_VERSION,
            "bootstrap_seed_digest":seed.digest,
            "bootstrap_seed_conversation_url":target_url
        }),
    )
}

async fn resolve_pending_new_chat(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    session_key: &str,
    target: &Value,
) -> Result<DeliveryOutcome, ApiError> {
    let target_url = target["url"].as_str().unwrap_or("");
    let conversation_url = state
        .browser_surfaces
        .wait_for_conversation_url(session_key, target_url, std::time::Duration::ZERO)
        .await
        .map_err(|error| {
            ApiError::unavailable("web_model_conversation_bind_failed", error.to_string())
        })?;
    let Some(conversation_url) = conversation_url else {
        update_target(
            state,
            group_id,
            actor_id,
            json!({"last_delivery_status":"pending_new_chat_bind","last_error":"conversation_url_pending"}),
        )?;
        return Ok(DeliveryOutcome::Ambiguous);
    };
    bind_new_chat_target(state, group_id, actor_id, &conversation_url)?;
    update_target(
        state,
        group_id,
        actor_id,
        json!({"last_delivery_status":"submitted","last_error":""}),
    )?;
    if let (Some(turn_id), Some(delivery_id), Some(event_ids)) = (
        target["last_delivery_turn_id"].as_str(),
        target["last_delivery_id"].as_str(),
        target["last_delivery_event_ids"].as_array(),
    ) {
        record_delivery(
            state,
            group_id,
            actor_id,
            turn_id,
            Value::Array(event_ids.clone()),
            delivery_id,
            "bound",
            "conversation_url_bound",
            json!({
                "target_url":target_url,
                "bound_conversation_url":conversation_url,
                "resolved_pending_new_chat":true
            }),
        )
        .await?;
    }
    Ok(DeliveryOutcome::Submitted)
}

fn bind_new_chat_target(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    conversation_url: &str,
) -> Result<(), ApiError> {
    let now = cccc_contracts::utc_now();
    update_target(
        state,
        group_id,
        actor_id,
        json!({
            "state":"bound_existing_chat",
            "kind":"existing_chat",
            "url":conversation_url,
            "saved_at":now,
            "bound_at":now,
            "next_delivery":"existing_chat",
            "bootstrap_seed_conversation_url":conversation_url
        }),
    )
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    value[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::bad(format!("runtime turn missing {key}")))
}

#[cfg(test)]
mod retry_integration_tests {
    use super::*;
    use cccc_core::{GroupStore, HomeLayout, ledger, web_model_connectors};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, timeout};

    #[test]
    fn rejected_session_guard_cannot_release_the_current_owner() {
        for registry in [&WORKERS, &IN_FLIGHT] {
            let key = format!("guard-contention-{}", uuid::Uuid::new_v4());
            let held = SessionGuard::acquire(registry, key.clone()).expect("first owner");
            for _ in 0..10 {
                assert!(
                    SessionGuard::acquire(registry, key.clone()).is_none(),
                    "rejected acquisition erased the live owner's registration"
                );
                assert!(
                    registry
                        .get()
                        .expect("registry")
                        .lock()
                        .expect("lock")
                        .contains(&key),
                    "a rejected contender released another worker's guard"
                );
            }
            drop(held);
            let next = SessionGuard::acquire(registry, key).expect("owner released normally");
            drop(next);
        }
    }

    #[tokio::test]
    async fn real_browser_deferral_resumes_the_same_report_once() {
        if crate::system_browser_path().is_none() {
            return;
        }
        let temp = tempfile::tempdir().expect("isolated test home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let (api, _, browser, state) = crate::app_with_shutdown(
            home.clone(),
            shutdown.clone(),
            crate::WebMode::Normal,
            None,
            crate::LiveBinding {
                host: "127.0.0.1".into(),
                port: 0,
            },
            "test-browser-retry".into(),
        );
        let daemon_home = home.clone();
        let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
        for _ in 0..100 {
            if daemon_call(&state, "ping", Default::default())
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("API listener");
        let api_url = format!("http://{}", api_listener.local_addr().expect("API address"));
        let api_server = tokio::spawn(async move {
            axum::serve(
                api_listener,
                api.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
        });
        let count = Arc::new(AtomicUsize::new(0));
        let received = Arc::clone(&count);
        let page = r#"<!doctype html><html><body>
<button style="position:fixed;left:10px;top:10px;width:130px;height:36px" type="button" onclick="document.querySelector('#busy').remove();this.remove()">Finish current answer</button>
<button style="position:fixed;left:350px;top:10px;width:130px;height:36px" type="button" onclick="document.querySelector('textarea').value=''">Resolve own test draft</button>
<button id="busy" type="button" aria-label="Stop streaming" style="position:fixed;left:180px;top:10px">Stop</button>
<textarea id="prompt-textarea" placeholder="Message" style="position:fixed;left:10px;top:70px;width:650px;height:120px">unsent human draft</textarea>
<button data-testid="send-button" type="button" aria-label="Send prompt" style="position:fixed;left:10px;top:230px;width:100px;height:35px" onclick="const t=document.querySelector('textarea');if(!t.value)return;const d=document.createElement('div');d.dataset.messageAuthorRole='user';d.textContent=t.value;d.style='margin-top:290px';document.body.append(d);t.value='';fetch('/received',{method:'POST'})">Send</button>
</body></html>"#;
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(move || async move { axum::response::Html(page) }),
            )
            .route(
                "/received",
                axum::routing::post(move || {
                    let received = Arc::clone(&received);
                    async move {
                        received.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let url = format!("http://{}/", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let work_state = state.clone();
        let work_home = home.clone();
        let work_browser = Arc::clone(&browser);
        let profile = temp.path().join("browser");
        let operation = async move {
            let state = work_state;
            let home = work_home;
            let browser = work_browser;
            let call = |op: &'static str, values: Value| {
                daemon_call(
                    &state,
                    op,
                    values.as_object().cloned().expect("test arguments"),
                )
            };
            let created = call("group_create", json!({"title":"real browser retry"}))
                .await
                .expect("create group");
            let gid = created["group"]["group_id"].as_str().expect("group id");
            call("actor_add",json!({"group_id":gid,"actor_id":"web","runtime":"web_model","by":"user","env":{"CCCC_WEB_MODEL_DELIVERY_MODE":"browser"}})).await.expect("actor");
            call(
                "actor_start",
                json!({"group_id":gid,"actor_id":"web","by":"user"}),
            )
            .await
            .expect("start");
            web_model_connectors::save_browser_target(
                &home,
                gid,
                "web",
                Some(json!({"kind":"existing_chat","url":url})),
            )
            .expect("local fixture target");
            let source=call("send",json!({"group_id":gid,"by":"user","to":["web"],"text":"BROWSER_RETRY_REPORT","message_mode":"mail"})).await.expect("Mail");
            let source_id = source["event"]["id"].as_str().expect("source id");
            call(
                "message_deliver",
                json!({"group_id":gid,"by":"user","source_event_id":source_id,"actor_ids":["web"]}),
            )
            .await
            .expect("promote");
            browser
                .ensure_open(surface_key(), &profile, &url, 800, 600)
                .await
                .expect("real Chrome");
            let busy = deliver_pending(&state, gid, "web")
                .await
                .expect("busy attempt");
            assert!(matches!(busy, DeliveryOutcome::Idle));
            assert_eq!(
                count.load(Ordering::SeqCst),
                0,
                "sent during the current answer"
            );
            let health: Value = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("HTTP client")
                .get(format!(
                    "{api_url}/api/v1/web-model/browser-session?group_id={gid}&actor_id=web"
                ))
                .send()
                .await
                .expect("native health endpoint")
                .json()
                .await
                .expect("health JSON");
            assert_eq!(
                health["result"]["health_snapshot"]["next_action"]["recommended"], "wait_for_reply",
                "busy browser was misleadingly reported as no action needed: {health}"
            );
            browser
                .command(surface_key(), &json!({"t":"click","x":75,"y":28}))
                .await
                .expect("finish fixture answer");
            let draft_wait = deliver_pending(&state, gid, "web")
                .await
                .expect("retained draft");
            assert!(matches!(draft_wait, DeliveryOutcome::Idle));
            assert_eq!(
                count.load(Ordering::SeqCst),
                0,
                "draft did not block submission"
            );
            let draft_health: Value = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("HTTP client")
                .get(format!(
                    "{api_url}/api/v1/web-model/browser-session?group_id={gid}&actor_id=web"
                ))
                .send()
                .await
                .expect("draft health endpoint")
                .json()
                .await
                .expect("draft health JSON");
            assert_eq!(
                draft_health["result"]["health_snapshot"]["next_action"]["recommended"],
                "resolve_draft"
            );
            browser
                .command(surface_key(), &json!({"t":"click","x":390,"y":28}))
                .await
                .expect("resolve isolated fixture draft");
            let delivered = deliver_pending(&state, gid, "web")
                .await
                .expect("resumed attempt");
            assert!(
                matches!(delivered, DeliveryOutcome::Submitted),
                "busy turn did not resume: target={} surface={} count={}",
                load_target(&state, gid, "web").expect("debug target"),
                browser.info(surface_key()).await,
                count.load(Ordering::SeqCst)
            );
            assert!(matches!(
                deliver_pending(&state, gid, "web")
                    .await
                    .expect("third attempt"),
                DeliveryOutcome::Idle
            ));
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "duplicate browser submission"
            );
            let store = GroupStore::new(home.clone()).expect("store");
            let events =
                ledger::read_all(&store.ledger_path(gid).expect("ledger")).expect("events");
            assert_eq!(
                events.iter().filter(|e| e.kind == "chat.message").count(),
                1
            );
            let transitions = events
                .iter()
                .filter(|e| e.kind == "runtime.delivery" && e.data["source_event_id"] == source_id)
                .filter_map(|e| e.data["state"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                transitions,
                vec![
                    "claimed", "failed", "claimed", "failed", "claimed", "accepted"
                ]
            );
            let mail = call(
                "inbox_peek",
                json!({"group_id":gid,"actor_id":"web","by":"web"}),
            )
            .await
            .expect("mail unchanged");
            assert_eq!(mail["messages"].as_array().expect("messages").len(), 1);
            let initial_browser = browser.info(surface_key()).await;
            for round in 2..=20 {
                let source = call(
                    "send",
                    json!({"group_id":gid,"by":"user","to":["web"],
                    "text":format!("CONTINUOUS_REPORT_{round}"),"message_mode":"mail"}),
                )
                .await
                .expect("next report");
                let event_id = source["event"]["id"].as_str().expect("next report id");
                call(
                    "message_deliver",
                    json!({"group_id":gid,"by":"user",
                    "source_event_id":event_id,"actor_ids":["web"]}),
                )
                .await
                .expect("next report promotion");
                assert!(matches!(
                    deliver_pending(&state, gid, "web")
                        .await
                        .expect("next handoff"),
                    DeliveryOutcome::Submitted
                ));
                assert!(matches!(
                    deliver_pending(&state, gid, "web")
                        .await
                        .expect("duplicate poll"),
                    DeliveryOutcome::Idle
                ));
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    round,
                    "duplicate or missing round {round}"
                );
                assert_eq!(
                    browser.info(surface_key()).await["started_at"],
                    initial_browser["started_at"],
                    "browser restarted for an ordinary next report"
                );
                assert!(
                    !IN_FLIGHT
                        .get()
                        .expect("guard store")
                        .lock()
                        .expect("guard lock")
                        .contains(&key(gid, "web")),
                    "completed turn retained the browser guard"
                );
            }
            let all =
                ledger::read_all(&store.ledger_path(gid).expect("ledger")).expect("all rounds");
            assert_eq!(all.iter().filter(|e| e.kind == "chat.message").count(), 20);
            assert_eq!(
                all.iter()
                    .filter(|e| e.kind == "runtime.delivery" && e.data["state"] == "accepted")
                    .count(),
                20
            );
            let unread = call(
                "inbox_peek",
                json!({"group_id":gid,"actor_id":"web","by":"web"}),
            )
            .await
            .expect("unread after rounds");
            assert_eq!(
                unread["messages"].as_array().expect("unread").len(),
                20,
                "delivery consumed Mail"
            );
            // Repeated real events call ensure_worker while one owner is polling.
            // Losing admission must not remove that owner or reset its idle cadence.
            browser
                .command(surface_key(), &json!({"t":"click","x":100,"y":110}))
                .await
                .expect("focus fixture composer");
            browser
                .command(surface_key(), &json!({"t":"text","text":"PRESERVE_DRAFT"}))
                .await
                .expect("fixture draft");
            let final_source=call("send",json!({"group_id":gid,"by":"user","to":["web"],"text":"CONTENDED_WORKER_REPORT","message_mode":"mail"})).await.expect("contended Mail");
            let final_id = final_source["event"]["id"].as_str().expect("event");
            call(
                "message_deliver",
                json!({"group_id":gid,"by":"user","source_event_id":final_id,"actor_ids":["web"]}),
            )
            .await
            .expect("promote contended Mail");
            for _ in 0..20 {
                ensure_worker(state.clone(), gid.to_owned(), "web".into()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let log =
                ledger::read_all(&store.ledger_path(gid).expect("ledger")).expect("worker events");
            let attempts = log
                .iter()
                .filter(|event| {
                    event.kind == "web_model.browser_delivery.submitting"
                        && event.data["event_ids"]
                            .as_array()
                            .is_some_and(|ids| ids.iter().any(|id| id == final_id))
                })
                .count();
            assert_eq!(
                attempts, 1,
                "duplicate worker admissions bypassed the existing idle retry interval"
            );
            assert_eq!(count.load(Ordering::SeqCst), 20, "draft was overwritten");
            browser
                .command(surface_key(), &json!({"t":"click","x":390,"y":28}))
                .await
                .expect("clear fixture draft");
            timeout(Duration::from_secs(8), async {
                while count.load(Ordering::SeqCst) != 21 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("original worker resumes queued Mail without another source message");
            assert_eq!(count.load(Ordering::SeqCst), 21);
            eprintln!(
                "REAL_CHROME_AND_DAEMON: 20 handoffs; 20 duplicate worker admissions preserve one polling owner; draft release delivers original report once"
            );
        };
        // Cleanup runs even if a test assertion panics in the task.
        let outcome = tokio::spawn(timeout(Duration::from_secs(40), operation)).await;
        let _ = browser.close(surface_key()).await;
        let _ = shutdown.send(());
        let _ = daemon_call(&state, "shutdown", Default::default()).await;
        let _ = timeout(Duration::from_secs(5), daemon).await;
        server.abort();
        api_server.abort();
        outcome
            .expect("browser flow assertions")
            .expect("bounded browser flow");
    }

    #[tokio::test]
    async fn two_group_ten_report_draft_wait_has_one_worker_per_group() {
        if crate::system_browser_path().is_none() {
            return;
        }
        let temp = tempfile::tempdir().expect("isolated home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let (_, _, browser, state) = crate::app_with_shutdown(
            home.clone(),
            shutdown.clone(),
            crate::WebMode::Normal,
            None,
            crate::LiveBinding {
                host: "127.0.0.1".into(),
                port: 0,
            },
            "two-group-browser".into(),
        );
        let daemon_home = home.clone();
        let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
        for _ in 0..100 {
            if daemon_call(&state, "ping", Default::default())
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let records = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&records);
        let page = r#"<!doctype html><body>
<button type="button" style="position:fixed;left:350px;top:10px;width:130px;height:36px" onclick="document.querySelector('textarea').value=''">Clear fixture draft</button>
<form><textarea id="prompt-textarea" style="position:fixed;left:10px;top:70px;width:650px;height:120px"></textarea>
<button type="button" aria-label="Send prompt" style="position:fixed;left:10px;top:230px;width:100px;height:35px" onclick="const t=document.querySelector('textarea');if(!t.value)return;const p=t.value;t.value='';const messages=JSON.parse(localStorage.getItem(location.pathname)||'[]');messages.push(p);localStorage.setItem(location.pathname,JSON.stringify(messages));render(p);fetch('/received',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({path:location.pathname,prompt:p})})">Send</button></form>
<script>function render(p){const d=document.createElement('div');d.dataset.messageAuthorRole='user';d.textContent=p;d.style='margin-top:300px';document.body.append(d)}JSON.parse(localStorage.getItem(location.pathname)||'[]').forEach(render);</script></body>"#;
        let app = axum::Router::new()
            .route(
                "/a",
                axum::routing::get(move || async move { axum::response::Html(page) }),
            )
            .route(
                "/b",
                axum::routing::get(move || async move { axum::response::Html(page) }),
            )
            .route(
                "/received",
                axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
                    let sink = Arc::clone(&sink);
                    async move {
                        sink.lock().expect("record lock").push(value);
                        "ok"
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let work_state = state.clone();
        let work_home = home.clone();
        let work_browser = Arc::clone(&browser);
        let profile = temp.path().join("browser");
        let operation = async move {
            let state = work_state;
            let home = work_home;
            let browser = work_browser;
            let call = |op: &'static str, value: Value| {
                daemon_call(&state, op, value.as_object().cloned().expect("request"))
            };
            let mut groups = Vec::new();
            for label in ["a", "b"] {
                let created = call("group_create", json!({"title":format!("fixture-{label}")}))
                    .await
                    .expect("group");
                let gid = created["group"]["group_id"]
                    .as_str()
                    .expect("group ID")
                    .to_owned();
                call("actor_add",json!({"group_id":gid,"actor_id":"web","runtime":"web_model","by":"user","env":{"CCCC_WEB_MODEL_DELIVERY_MODE":"browser"}})).await.expect("actor");
                call(
                    "actor_start",
                    json!({"group_id":gid,"actor_id":"web","by":"user"}),
                )
                .await
                .expect("start");
                web_model_connectors::save_browser_target(
                    &home,
                    &gid,
                    "web",
                    Some(json!({"kind":"existing_chat","url":format!("{base}/{label}")})),
                )
                .expect("target");
                groups.push((label, gid));
            }
            browser
                .ensure_open(surface_key(), &profile, &format!("{base}/a"), 800, 600)
                .await
                .expect("one browser");
            let initial_browser = browser.info(surface_key()).await;
            browser
                .command(surface_key(), &json!({"t":"click","x":100,"y":110}))
                .await
                .expect("focus");
            browser
                .command(
                    surface_key(),
                    &json!({"t":"text","text":"PRESERVE_GROUP_A_DRAFT"}),
                )
                .await
                .expect("draft");
            let mut sources = Vec::new();
            for round in 0..5 {
                for (label, gid) in &groups {
                    let marker = format!("ROUTED_{label}_{round}");
                    let event=call("send",json!({"group_id":gid,"by":"user","to":["web"],"text":marker,"message_mode":"mail"})).await.expect("report");
                    let id = event["event"]["id"].as_str().expect("event ID").to_owned();
                    call("message_deliver",json!({"group_id":gid,"by":"user","source_event_id":id,"actor_ids":["web"]})).await.expect("promote original");
                    sources.push((gid.clone(), id, marker));
                }
            }
            for _ in 0..10 {
                for (_, gid) in &groups {
                    ensure_worker(state.clone(), gid.clone(), "web".into()).await;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let store = GroupStore::new(home.clone()).expect("store");
            for (_, gid) in &groups {
                let events =
                    ledger::read_all(&store.ledger_path(gid).expect("ledger")).expect("events");
                let attempts = events
                    .iter()
                    .filter(|event| event.kind == "web_model.browser_delivery.submitting")
                    .count();
                assert_eq!(
                    attempts, 1,
                    "repeated admission bypassed the polling owner in {gid}"
                );
            }
            assert!(
                records.lock().expect("records").is_empty(),
                "B navigated or sent while A had a draft"
            );
            assert_eq!(
                browser.info(surface_key()).await["url"],
                format!("{base}/a")
            );
            browser
                .command(surface_key(), &json!({"t":"click","x":390,"y":28}))
                .await
                .expect("resolve own fixture draft");
            timeout(Duration::from_secs(12), async {
                loop {
                    if records.lock().expect("records").len() == 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("both original batches resume");
            // Wait until the native completion records, not just the page echoes, settle.
            timeout(Duration::from_secs(5), async {
                loop {
                    let accepted = groups
                        .iter()
                        .map(|(_, gid)| {
                            ledger::read_all(&store.ledger_path(gid).expect("ledger"))
                                .expect("events")
                                .into_iter()
                                .filter(|event| {
                                    event.kind == "runtime.delivery"
                                        && event.data["state"] == "accepted"
                                })
                                .count()
                        })
                        .sum::<usize>();
                    if accepted == 10 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("all ten reports accepted");
            let received = records.lock().expect("records").clone();
            for (label, gid) in &groups {
                let own = received
                    .iter()
                    .find(|item| item["path"] == format!("/{label}"))
                    .expect("own target received");
                let text = own["prompt"].as_str().expect("prompt");
                for (source_gid, id, marker) in &sources {
                    assert_eq!(
                        text.contains(marker),
                        source_gid == gid,
                        "wrong group received {marker}"
                    );
                    if source_gid == gid {
                        assert!(text.contains(id), "batch omitted original event ID");
                    }
                }
                let unread = call(
                    "inbox_peek",
                    json!({"group_id":gid,"actor_id":"web","by":"web"}),
                )
                .await
                .expect("inbox");
                assert_eq!(unread["messages"].as_array().expect("messages").len(), 5);
            }
            for _ in 0..10 {
                for (_, gid) in &groups {
                    ensure_worker(state.clone(), gid.clone(), "web".into()).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                records.lock().expect("records").len(),
                2,
                "accepted batches were sent again"
            );
            assert_eq!(
                browser.info(surface_key()).await["started_at"],
                initial_browser["started_at"]
            );
            eprintln!(
                "TWO_GROUP_REAL_CHROME: ten original reports; same-named actors isolated; A draft preserved; one polling owner each; two batches resumed; ten accepted once; Mail unread"
            );
        };
        let result = tokio::spawn(timeout(Duration::from_secs(35), operation)).await;
        let _ = shutdown.send(());
        let _ = browser.close(surface_key()).await;
        let _ = daemon_call(&state, "shutdown", Default::default()).await;
        let _ = timeout(Duration::from_secs(5), daemon).await;
        server.abort();
        result
            .expect("test assertions")
            .expect("bounded two-group flow");
    }
}
