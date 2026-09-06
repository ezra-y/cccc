use super::{ActiveTurn, Session, events};
use serde_json::{Map, Value, json};

pub(super) fn handle_message(session: &Session, message: Value) {
    if message.get("id").is_some() {
        if message.get("method").and_then(Value::as_str).is_some() {
            respond_unsupported_server_request(session, &message);
        }
        return;
    }
    handle_announced_message(session, message);
}

fn respond_unsupported_server_request(session: &Session, message: &Value) {
    let Some(id) = message.get("id") else {
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let _ = session.respond_error(
        id.clone(),
        json!({
            "code":-32601,
            "message":format!("CCCC headless does not support provider request: {method}")
        }),
    );
}

fn handle_announced_message(session: &Session, message: Value) {
    if message.get("method").and_then(Value::as_str) == Some("turn/started") {
        handle_managed_turn_started(session, &message);
        return;
    }
    let completed = message.get("method").and_then(Value::as_str) == Some("turn/completed");
    if completed {
        complete_turn(session, &message);
        return;
    }
    if message.get("method").and_then(Value::as_str) == Some("thread/status/changed") {
        let flags = message
            .pointer("/params/status/activeFlags")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let waiting = flags.iter().any(|flag| {
            matches!(
                flag.as_str(),
                Some("waitingOnApproval" | "waitingOnUserInput")
            )
        });
        let task = active_context(session);
        if waiting {
            session.set_status("waiting", task);
        } else if message
            .pointer("/params/status/type")
            .and_then(Value::as_str)
            == Some("active")
            && task.is_some()
            && session
                .status
                .lock()
                .is_ok_and(|state| state.status == "waiting")
        {
            session.set_status("working", task);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartedTurnDisposition {
    Adopted,
    Matched,
    Conflict,
}

fn handle_managed_turn_started(session: &Session, message: &Value) {
    let turn_id = message
        .pointer("/params/turn/id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(turn_id) = turn_id else { return };
    match observe_started_turn(&session.active_turn, turn_id) {
        StartedTurnDisposition::Adopted => {
            session.set_status("working", Some(turn_id.to_owned()));
        }
        StartedTurnDisposition::Matched => {}
        StartedTurnDisposition::Conflict => {
            tracing::warn!(
                group_id = %session.group_id,
                actor_id = %session.actor_id,
                turn_id,
                "managed Actor reported an overlapping terminal turn; stopping the inconsistent session"
            );
            let _ = session.stop();
        }
    }
}

fn observe_started_turn(
    active_turn: &std::sync::Mutex<Option<ActiveTurn>>,
    turn_id: &str,
) -> StartedTurnDisposition {
    let Ok(mut active_turn) = active_turn.lock() else {
        return StartedTurnDisposition::Conflict;
    };
    match active_turn.as_mut() {
        Some(active) if active.turn_id == turn_id => StartedTurnDisposition::Matched,
        Some(_) => StartedTurnDisposition::Conflict,
        None => {
            *active_turn = Some(ActiveTurn {
                turn_id: turn_id.to_owned(),
                started_at: cccc_contracts::utc_now(),
            });
            StartedTurnDisposition::Adopted
        }
    }
}

/// A lost provider stream is a failed handoff, not a completed user task.
/// Reuse normal report promotion/deduplication; an idle or intentionally stopped
/// session has no unfinished turn to report.
pub(super) fn fail_active_turn(session: &Session, reason: &str) {
    if session.stopped.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(turn_id) = active_context(session) else {
        return;
    };
    complete_turn(
        session,
        &json!({"params":{"turn":{
            "id":turn_id,"status":"failed","error":{"message":reason}
        }}}),
    );
}

fn complete_turn(session: &Session, message: &Value) {
    let Ok(mut active_turn) = session.active_turn.lock() else {
        return;
    };
    let Some(current) = active_turn.as_ref() else {
        return;
    };
    let reported_turn_id = message
        .pointer("/params/turn/id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !reported_turn_id.is_empty()
        && !current.turn_id.is_empty()
        && reported_turn_id != current.turn_id
    {
        return;
    }
    let finished = active_turn.take().expect("matched active turn");
    drop(active_turn);
    session.set_status("idle", None);
    if let Err(error) = notify_web_foreman(
        &session.home,
        &session.group_id,
        &session.actor_id,
        &finished,
        message,
    ) {
        tracing::error!(%error, group_id=%session.group_id, actor_id=%session.actor_id,
            turn_id=%finished.turn_id, "failed to record local member completion notice");
    }
}

// Completion is a handoff fact, never a decision that the overall task is done.
// Reuse the existing ledger, send operation and idempotency key; no new loop.
fn notify_web_foreman(
    home: &cccc_core::HomeLayout,
    group_id: &str,
    actor_id: &str,
    turn: &ActiveTurn,
    message: &Value,
) -> std::io::Result<()> {
    use cccc_contracts::{ActorRuntime, DaemonRequest, GroupState};
    use cccc_core::{GroupStore, actors, ledger};
    let store = GroupStore::new(home.clone())?;
    let group = store.load(group_id)?;
    let delivery_enabled =
        group.running && !matches!(group.state, GroupState::Paused | GroupState::Stopped);
    let Ok(lead) = actors::unique_available_foreman(&group) else {
        return Ok(());
    };
    if lead.runtime != ActorRuntime::WebModel || lead.id == actor_id {
        return Ok(());
    }
    let status = message
        .pointer("/params/turn/status")
        .and_then(Value::as_str)
        .unwrap_or("ended");
    // ponytail: bounded scan may produce an extra reminder on extremely busy groups;
    // use indexed turn ownership if the group exceeds 2,000 messages per member turn.
    let (recent, _) =
        ledger::tail_filtered(&store.ledger_path(group_id)?, 2_000, Some("chat.message"))?;
    let reports = recent
        .iter()
        .filter(|event| {
            event.by == actor_id
                && event.ts >= turn.started_at
                && event
                    .data
                    .get("to")
                    .and_then(Value::as_array)
                    .is_some_and(|to| to.iter().any(|id| id.as_str() == Some(lead.id.as_str())))
        })
        .cloned()
        .collect::<Vec<_>>();
    if !reports.is_empty() {
        if delivery_enabled {
            for report in &reports {
                if report.data.get("message_mode").and_then(Value::as_str) != Some("mail") {
                    continue;
                }
                // Reuse every visible report. Mail alone cannot wake an idle browser
                // Foreman; native delivery promotion preserves both message and cursor.
                let request=DaemonRequest {v:1,op:"message_deliver".into(),args:json!({
                    "group_id":group_id,"by":actor_id,"source_event_id":report.id,"actor_ids":[lead.id]
                }).as_object().cloned().expect("report delivery request")};
                let result = super::super::messaging::handle(home, &request)
                    .expect("native message delivery");
                if let Err(error) = result {
                    if !matches!(
                        error.code.as_str(),
                        "already_delivered" | "delivery_in_progress" | "delivery_ambiguous"
                    ) {
                        return Err(std::io::Error::other(format!(
                            "{}: {}",
                            error.code, error.message
                        )));
                    }
                }
            }
        }
        super::super::coordination_relay::record_handoff(
            home,
            &group,
            actor_id,
            &lead.id,
            &turn.turn_id,
            &reports,
            status,
        )
        .map_err(|error| std::io::Error::other(format!("{}: {}", error.code, error.message)))?;
        return Ok(());
    }
    if !delivery_enabled {
        return Ok(());
    }
    let key = super::super::message_idempotency::tracked_client_id(
        group_id,
        actor_id,
        &format!("completion:{}", turn.turn_id),
    );
    let request = DaemonRequest { v:1, op:"send".into(), args:json!({
        "group_id":group_id,"by":"system","to":[lead.id],"message_mode":"send","client_id":key,
        "text":format!("[CCCC] Member {actor_id} ended this turn ({status}). Please review its actual result and decide the next step; this notice does not mark the task complete."),
        "source_actor_id":actor_id,"source_turn_id":turn.turn_id
    }).as_object().cloned().expect("completion request") };
    let sent = super::super::messaging::send(home, &request, "chat.message")
        .map_err(|error| std::io::Error::other(format!("{}: {}", error.code, error.message)))?;
    let event: cccc_contracts::Event =
        serde_json::from_value(sent.get("event").cloned().unwrap_or(Value::Null))
            .map_err(std::io::Error::other)?;
    super::super::coordination_relay::record_handoff(
        home,
        &group,
        actor_id,
        &lead.id,
        &turn.turn_id,
        &[event],
        status,
    )
    .map(|_| ())
    .map_err(|error| std::io::Error::other(format!("{}: {}", error.code, error.message)))
}

fn active_context(session: &Session) -> Option<String> {
    session
        .active_turn
        .lock()
        .ok()?
        .as_ref()
        .map(|turn| turn.turn_id.clone())
}

pub(super) fn emit(session: &Session, kind: &str, data: Map<String, Value>) {
    if let Err(error) = events::append(
        &session.home,
        &session.group_id,
        &session.actor_id,
        kind,
        data,
    ) {
        tracing::warn!(%error, group_id = %session.group_id, actor_id = %session.actor_id, "failed to append headless event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untracked_codex_turn_is_adopted_until_its_completion() {
        let active_turn = std::sync::Mutex::new(None);

        assert_eq!(
            observe_started_turn(&active_turn, "turn-terminal"),
            StartedTurnDisposition::Adopted
        );
        let active = active_turn.lock().expect("active turn");
        let active = active.as_ref().expect("adopted turn");
        assert_eq!(active.turn_id, "turn-terminal");
    }

    #[test]
    fn a_repeated_started_event_matches_the_active_turn_but_not_an_overlap() {
        let active_turn = std::sync::Mutex::new(Some(ActiveTurn {
            turn_id: "turn-terminal".into(),
            started_at: cccc_contracts::utc_now(),
        }));

        assert_eq!(
            observe_started_turn(&active_turn, "turn-terminal"),
            StartedTurnDisposition::Matched
        );
        assert_eq!(
            observe_started_turn(&active_turn, "turn-overlap"),
            StartedTurnDisposition::Conflict
        );
    }
    #[test]
    fn multiple_member_outputs_remain_visible_and_share_one_durable_handoff() {
        use cccc_contracts::{Actor, ActorRuntime, Event};
        use cccc_core::{GroupStore, HomeLayout, actors, ledger};
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("multiple reports", "").expect("group");
        let mut lead = Actor::new("web-lead");
        lead.runtime = ActorRuntime::WebModel;
        actors::add(&mut group, lead).expect("lead");
        actors::add(&mut group, Actor::new("worker")).expect("worker");
        group.running = true;
        store.save(&group).expect("save");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let turn = ActiveTurn {
            turn_id: "turn-multi".into(),
            started_at: cccc_contracts::utc_now(),
        };
        let mut source_ids = Vec::new();
        for (index, mode) in ["mail", "send"].into_iter().enumerate() {
            let mut report = Event::new("chat.message", &group.group_id);
            report.by = "worker".into();
            report.data = json!({
                "to":["web-lead"],"message_mode":mode,
                "text":format!("Human-visible output {}", index + 1)
            })
            .as_object()
            .cloned()
            .expect("report");
            source_ids.push(report.id.clone());
            ledger::append(&path, &report).expect("append report");
        }
        let completed = json!({"params":{"turn":{"id":"turn-multi","status":"completed"}}});
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed)
            .expect("complete multi-report turn");
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed)
            .expect("repeat completion");
        let events = ledger::read_all(&path).expect("ledger");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "chat.message")
                .count(),
            2,
            "visible outputs were duplicated or replaced"
        );
        let handoffs = events
            .iter()
            .filter(|event| event.kind == "coordination.handoff")
            .collect::<Vec<_>>();
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].data["source_event_ids"], json!(source_ids));
        assert!(events.iter().any(|event| {
            event.kind == "runtime.delivery"
                && event.data.get("source_event_id") == Some(&json!(source_ids[0]))
        }));
    }

    #[test]
    fn completion_notice_is_durable_deduplicated_and_does_not_replace_a_report() {
        use cccc_contracts::{Actor, ActorRuntime, Event, GroupState};
        use cccc_core::{GroupStore, HomeLayout, actors, ledger};
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("handoff", "").expect("group");
        let mut lead = Actor::new("web-lead");
        lead.runtime = ActorRuntime::WebModel;
        actors::add(&mut group, lead).expect("lead");
        actors::add(&mut group, Actor::new("worker")).expect("worker");
        group.running = true;
        store.save(&group).expect("save");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let turn = ActiveTurn {
            turn_id: "turn-one".into(),
            started_at: cccc_contracts::utc_now(),
        };
        let completed = json!({"params":{"turn":{"id":"turn-one","status":"completed"}}});
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed).expect("notice");
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed).expect("duplicate");
        let notices = ledger::tail_filtered(&path, 100, Some("chat.message"))
            .expect("messages")
            .0;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].data["to"], json!(["web-lead"]));
        assert_eq!(notices[0].data["message_mode"], "send");
        let first_handoffs = ledger::tail_filtered(&path, 100, Some("coordination.handoff"))
            .expect("handoffs")
            .0;
        assert_eq!(first_handoffs.len(), 1);
        assert_eq!(
            first_handoffs[0].data["source_event_ids"],
            json!([notices[0].id])
        );
        assert_eq!(first_handoffs[0].data["target_actor_id"], "web-lead");
        let turn = ActiveTurn {
            turn_id: "turn-two".into(),
            started_at: cccc_contracts::utc_now(),
        };
        let mut report = Event::new("chat.message", &group.group_id);
        report.by = "worker".into();
        report.data = json!({"to":["web-lead"],"message_mode":"send","text":"Actual result"})
            .as_object()
            .cloned()
            .expect("report");
        ledger::append(&path, &report).expect("report event");
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed)
            .expect("already reported");
        assert_eq!(
            ledger::tail_filtered(&path, 100, Some("chat.message"))
                .expect("messages")
                .0
                .len(),
            2
        );
        let mailed_turn = ActiveTurn {
            turn_id: "turn-mail".into(),
            started_at: cccc_contracts::utc_now(),
        };
        let mut mailed = Event::new("chat.message", &group.group_id);
        mailed.by = "worker".into();
        mailed.data =
            json!({"to":["web-lead"],"message_mode":"mail","text":"Final report by mail"})
                .as_object()
                .cloned()
                .expect("mail");
        ledger::append(&path, &mailed).expect("mail event");
        notify_web_foreman(&home, &group.group_id, "worker", &mailed_turn, &completed)
            .expect("promote mail");
        notify_web_foreman(&home, &group.group_id, "worker", &mailed_turn, &completed)
            .expect("promotion repeat");
        let events = ledger::tail(&path, 100).expect("ledger");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "chat.message")
                .count(),
            3,
            "mail promotion duplicated the report"
        );
        assert!(
            events.iter().any(|event| event.kind == "runtime.delivery"
                && event.data.get("source_event_id") == Some(&json!(mailed.id))),
            "mailed final report never entered the native delivery path"
        );
        let handoffs = events
            .iter()
            .filter(|event| event.kind == "coordination.handoff")
            .collect::<Vec<_>>();
        assert_eq!(handoffs.len(), 3, "one durable handoff per completed turn");
        assert!(
            handoffs
                .iter()
                .any(|event| event.data["source_event_ids"] == json!([report.id]))
        );
        assert!(
            handoffs
                .iter()
                .any(|event| event.data["source_event_ids"] == json!([mailed.id]))
        );
        let context = cccc_core::context::ContextStore::new(home.clone())
            .expect("context store")
            .load(&group.group_id)
            .expect("context");
        assert_eq!(
            context.coordination["recent_handoffs"]
                .as_array()
                .expect("handoff notes")
                .len(),
            3
        );
        group.state = GroupState::Paused;
        store.save(&group).expect("pause");
        let turn = ActiveTurn {
            turn_id: "turn-three".into(),
            started_at: cccc_contracts::utc_now(),
        };
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed).expect("paused");
        assert_eq!(
            ledger::tail_filtered(&path, 100, Some("chat.message"))
                .expect("messages")
                .0
                .len(),
            3
        );
    }

    #[test]
    fn paused_completion_preserves_an_existing_report_as_a_handoff_without_waking() {
        use cccc_contracts::{Actor, ActorRuntime, Event, GroupState};
        use cccc_core::{GroupStore, HomeLayout, actors, ledger};
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("group store");
        let mut group = store.create("paused report", "").expect("group");
        let mut lead = Actor::new("web-lead");
        lead.runtime = ActorRuntime::WebModel;
        actors::add(&mut group, lead).expect("lead");
        actors::add(&mut group, Actor::new("worker")).expect("worker");
        group.running = true;
        group.state = GroupState::Paused;
        store.save(&group).expect("save group");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let turn = ActiveTurn {
            turn_id: "turn-paused-report".into(),
            started_at: cccc_contracts::utc_now(),
        };
        let mut report = Event::new("chat.message", &group.group_id);
        report.by = "worker".into();
        report.data = json!({
            "to":["web-lead"],"message_mode":"mail",
            "text":"The paused work produced a real result that must survive resume."
        })
        .as_object()
        .cloned()
        .expect("report");
        ledger::append(&path, &report).expect("append report");
        let completed = json!({"params":{"turn":{"id":"turn-paused-report","status":"completed"}}});
        notify_web_foreman(&home, &group.group_id, "worker", &turn, &completed)
            .expect("paused completion");
        let events = ledger::read_all(&path).expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "chat.message")
                .count(),
            1,
            "pause added a duplicate visible message"
        );
        let handoff = events
            .iter()
            .find(|event| event.kind == "coordination.handoff")
            .expect("durable handoff");
        assert_eq!(handoff.data["source_event_ids"], json!([report.id]));
        assert!(
            !events.iter().any(|event| {
                event.kind == "runtime.delivery"
                    && event.data.get("source_event_id") == Some(&json!(report.id))
            }),
            "pause woke the browser instead of only preserving responsibility"
        );
    }
}
