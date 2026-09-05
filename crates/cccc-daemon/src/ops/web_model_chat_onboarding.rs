//! Chat-first provisioning, serialized by the daemon's existing global write gate.
//! Reuses group/actor operations and the connector store; it never starts a second server.
use cccc_contracts::{ActorRuntime, DaemonRequest};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, actors, web_model_connectors};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::dispatch::{OpError, OpResult, object, required_arg};

pub fn handle(home: &HomeLayout, request: &DaemonRequest) -> Option<OpResult> {
    match request.op.as_str() {
        "web_model_chat_create" => Some(onboard(home, request, true)),
        "web_model_chat_bind" => Some(onboard(home, request, false)),
        _ => None,
    }
}

fn op(home: &HomeLayout, name: &str, args: Value) -> OpResult {
    let request = DaemonRequest {
        v: 1,
        op: name.into(),
        args: args.as_object().cloned().expect("internal request"),
    };
    super::group_creation::handle(home, &request)
        .or_else(|| super::groups::handle(home, &request))
        .or_else(|| super::actors::handle(home, &request))
        .expect("known provisioning operation")
}

fn workspace(group: &GroupDoc) -> Option<&str> {
    group
        .scopes
        .iter()
        .find(|scope| scope.scope_key == group.active_scope_key)
        .map(|scope| scope.url.as_str())
}

fn exact_group(store: &GroupStore, token: &str) -> Result<GroupDoc, OpError> {
    if let Ok(group) = store.load(token) {
        return Ok(group);
    }
    let matches = store
        .list()
        .map_err(OpError::io)?
        .into_iter()
        .filter(|meta| meta.title.trim() == token)
        .map(|meta| store.load(&meta.group_id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(OpError::io)?;
    match matches.as_slice() {
        [group] => Ok(group.clone()),
        [] => Err(OpError::new(
            "group_not_found",
            "Use an exact group ID or a unique complete group name",
        )),
        _ => Err(OpError::new("ambiguous_group", "Use the exact group ID")),
    }
}

fn onboard(home: &HomeLayout, request: &DaemonRequest, create: bool) -> OpResult {
    if request.args.get("by").and_then(Value::as_str) != Some("user") {
        return Err(OpError::new(
            "permission_denied",
            "Chat onboarding requires the trusted gateway control plane",
        ));
    }
    let session = required_arg(request, "session")?;
    let allowed = if create {
        &["by", "session", "path", "title", "chat_url"][..]
    } else {
        &["by", "session", "group", "chat_url"][..]
    };
    if request
        .args
        .iter()
        .any(|(key, value)| !allowed.contains(&key.as_str()) || !value.is_string())
    {
        return Err(OpError::new(
            "invalid_request",
            "Only documented string arguments are accepted",
        ));
    }
    let url = request
        .args
        .get("chat_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let url = if url.is_empty() {
        None
    } else {
        Some(
            web_model_connectors::normalized_chatgpt_conversation_url(url).ok_or_else(|| {
                OpError::new(
                    "invalid_chat_url",
                    "Use this ChatGPT conversation's stable https://chatgpt.com/c/... URL",
                )
            })?,
        )
    };
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let bound = web_model_connectors::find_session(home, &session).map_err(OpError::io)?;
    let mut created_group = false;
    let mut new_directory = None::<PathBuf>;
    let group = if create {
        let raw = required_arg(request, "path")?;
        if !(Path::new(&raw).is_absolute() || raw.starts_with("~/")) {
            return Err(OpError::new(
                "invalid_scope_path",
                "Use an absolute project directory",
            ));
        }
        let expanded = cccc_core::path_input::expand_user_path(&raw).map_err(OpError::io)?;
        let path =
            if expanded.exists() {
                expanded.canonicalize().map_err(OpError::io)?
            } else {
                expanded
                    .parent()
                    .ok_or_else(|| OpError::new("invalid_scope_path", "Project parent is missing"))?
                    .canonicalize()
                    .map_err(OpError::io)?
                    .join(expanded.file_name().ok_or_else(|| {
                        OpError::new("invalid_scope_path", "Project name is missing")
                    })?)
            };
        if path.starts_with(home.root()) {
            return Err(OpError::new(
                "invalid_scope_path",
                "Choose a project directory outside CCCC's internal data directory",
            ));
        }
        let title = request
            .args
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        if let Some(binding) = &bound {
            let group = store
                .load(binding["group_id"].as_str().unwrap_or_default())
                .map_err(OpError::io)?;
            if workspace(&group).map(Path::new) != Some(path.as_path()) || group.title != title {
                return Err(OpError::new(
                    "session_already_bound",
                    "This Chat already belongs to another group; use its existing group",
                ));
            }
            group
        } else {
            if !path.exists() {
                new_directory = Some(path.clone());
            }
            let result = op(
                home,
                "group_create_with_scope",
                json!({"path":path,"title":title,"by":"user","set_active":false}),
            )?;
            let id = result["group_id"].as_str().ok_or_else(|| {
                OpError::new("group_unavailable", "Creation returned no group ID")
            })?;
            created_group = true;
            store.load(id).map_err(OpError::io)?
        }
    } else {
        exact_group(&store, &required_arg(request, "group")?)?
    };
    let result = connect(
        home,
        &store,
        &group,
        &session,
        url.as_deref(),
        bound.as_ref(),
        !created_group,
    );
    if let Err(error) = result {
        if created_group {
            if let Err(cleanup) = op(
                home,
                "group_delete",
                json!({"group_id":group.group_id,"by":"user"}),
            ) {
                return Err(OpError::new(
                    "onboarding_cleanup_failed",
                    format!(
                        "{}; could not remove incomplete group: {}",
                        error.message, cleanup.message
                    ),
                ));
            }
            if let Some(path) = new_directory {
                if let Err(cleanup) = std::fs::remove_dir(&path) {
                    return Err(OpError::new(
                        "onboarding_cleanup_failed",
                        format!(
                            "{}; could not remove newly created empty directory: {cleanup}",
                            error.message
                        ),
                    ));
                }
            }
        }
        return Err(error);
    }
    result
}

fn connect(
    home: &HomeLayout,
    store: &GroupStore,
    group: &GroupDoc,
    session: &str,
    url: Option<&str>,
    bound: Option<&Value>,
    reused: bool,
) -> OpResult {
    if bound.is_some_and(|binding| binding["group_id"] != group.group_id) {
        return Err(OpError::new(
            "session_already_bound",
            "This conversation is bound to another group",
        ));
    }
    if workspace(group).is_none() {
        return Err(OpError::new(
            "workspace_required",
            "Attach a project directory to this group first",
        ));
    }
    // A bound Chat operates its actual member, including peers under a local leader.
    // Only an unbound onboarding request uses the default Foreman flow.
    let member = if let Some(binding) = bound {
        let id = binding["actor_id"].as_str().unwrap_or_default();
        Some(actors::find(group, id).ok_or_else(|| {
            OpError::new(
                "connector_actor_unavailable",
                "The bound member no longer exists",
            )
        })?)
    } else {
        actors::visible(group).next()
    };
    if member.is_some_and(|actor| actor.runtime != ActorRuntime::WebModel) {
        return Err(OpError::new(
            "foreman_conflict",
            "This group already has a local Foreman; change it explicitly in the group interface",
        ));
    }
    let actor_id = member
        .map(|actor| actor.id.as_str())
        .unwrap_or("chat-foreman");
    let existing = web_model_connectors::load(home)
        .map_err(OpError::io)?
        .into_iter()
        .find(|entry| {
            entry["group_id"] == group.group_id
                && entry["actor_id"] == actor_id
                && entry["revoked"] != true
        });
    let same_owner = existing
        .as_ref()
        .zip(bound)
        .is_some_and(|(entry, binding)| entry["connector_id"] == binding["connector_id"]);
    if !same_owner
        && existing.as_ref().is_some_and(|entry| {
            entry["session_hash"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        })
    {
        return Err(OpError::new(
            "group_already_bound",
            "This group belongs to another Chat. Use a freshly issued connection code to replace that binding",
        ));
    }
    let created_actor = member.is_none();
    if created_actor {
        op(
            home,
            "actor_add",
            json!({"group_id":group.group_id,"actor_id":actor_id,
            "title":"ChatGPT Foreman","runtime":"web_model","by":"user",
            "env":{"CCCC_WEB_MODEL_PROVIDER":"chatgpt","CCCC_WEB_MODEL_DELIVERY_MODE":"browser"}}),
        )?;
    }
    let mut created_connector = None::<String>;
    let result = (|| -> OpResult {
        let connector = if let Some(entry) = existing {
            entry
        } else {
            let (entry, _) = web_model_connectors::create(
                home,
                &group.group_id,
                actor_id,
                "chatgpt",
                member
                    .map(|actor| actor.title.as_str())
                    .unwrap_or("ChatGPT Foreman"),
            )
            .map_err(OpError::io)?;
            created_connector = entry["connector_id"].as_str().map(str::to_owned);
            entry
        };
        let connector_id = connector["connector_id"]
            .as_str()
            .ok_or_else(|| OpError::new("connector_unavailable", "Missing connector identity"))?;
        // Complete all fallible group writes before switching the inbound binding.
        op(
            home,
            "actor_start",
            json!({"group_id":group.group_id,"actor_id":actor_id,"by":"user"}),
        )?;
        if let Some(url) = url {
            web_model_connectors::save_browser_target(
                home,
                &group.group_id,
                actor_id,
                Some(json!({
                    "kind":"existing_chat","state":"bound_existing_chat","url":url,
                    "saved_at":cccc_contracts::utc_now(),"next_delivery":"existing_chat"
                })),
            )
            .map_err(OpError::io)?;
        }
        if !same_owner {
            let binding = web_model_connectors::prepare_binding(home, connector_id, 600)
                .map_err(OpError::io)?;
            web_model_connectors::bind_session(
                home,
                connector_id,
                binding["code"].as_str().unwrap_or_default(),
                session,
            )
            .map_err(OpError::io)?;
        }
        let current = store.load(&group.group_id).map_err(OpError::io)?;
        let target = web_model_connectors::browser_target(home, &group.group_id, actor_id)
            .map_err(OpError::io)?;
        let callback_ready = target["kind"] == "existing_chat"
            && target["url"]
                .as_str()
                .and_then(web_model_connectors::normalized_chatgpt_conversation_url)
                .is_some();
        object(
            json!({"group_id":current.group_id,"group_title":current.title,"actor_id":actor_id,
                "role":actors::effective_role(&current,actor_id),"workspace":workspace(&current),"inbound_bound":true,"can_dispatch":true,
                "callback_target_ready":callback_ready,"status":if callback_ready{"configured"}else{"needs_chat_url"},
                "reused":reused,"members":actors::visible(&current).map(|actor|json!({"actor_id":actor.id,"title":actor.title,"runtime":actor.runtime,"role":actors::effective_role(&current,&actor.id)})).collect::<Vec<_>>(),
                "next_steps":if callback_ready {vec!["Call cccc_bootstrap. The callback target is saved; browser sign-in and an actual delivery still need verification."]}
                else{vec!["Call cccc_bootstrap to work with this group. Supply this Chat's stable /c/... URL to cccc_group_bind before expecting browser return messages."]}
            }),
        )
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        if let Some(id) = created_connector {
            if let Err(cleanup) = web_model_connectors::revoke(home, &id) {
                failures.push(cleanup.to_string());
            }
        }
        if created_actor {
            if let Err(cleanup) = op(
                home,
                "actor_remove",
                json!({"group_id":group.group_id,"actor_id":actor_id,"by":"user"}),
            ) {
                failures.push(cleanup.message);
            }
        }
        if failures.is_empty() {
            return Err(error);
        }
        return Err(OpError::new(
            "onboarding_cleanup_failed",
            format!("{}; {}", error.message, failures.join("; ")),
        ));
    }
    result
}
