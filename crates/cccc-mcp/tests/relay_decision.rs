use cccc_client::DaemonClient;
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event};
use cccc_core::{GroupStore, HomeLayout, actors, ledger};
use serde_json::{Map, Value, json};
use std::time::Duration;

fn request(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}
    })
}

fn payload(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

#[tokio::test]
async fn mcp_read_exposes_pending_handoff_and_explicit_decision_resolves_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("relay MCP", "").expect("group");
    let mut lead = Actor::new("web-lead");
    lead.runtime = ActorRuntime::WebModel;
    actors::add(&mut group, lead).expect("lead");
    actors::add(&mut group, Actor::new("worker")).expect("worker");
    group.running = true;
    store.save(&group).expect("save group");
    let path = store.ledger_path(&group.group_id).expect("ledger path");

    let mut report = Event::new("chat.message", &group.group_id);
    report.by = "worker".into();
    report.data = json!({
        "to":["web-lead"],"message_mode":"mail",
        "text":"The implementation is ready. The user must choose the release window."
    })
    .as_object()
    .cloned()
    .expect("report");
    ledger::append(&path, &report).expect("report");
    let mut handoff = Event::new("coordination.handoff", &group.group_id);
    handoff.by = "worker".into();
    handoff.data = json!({
        "handoff_id":"handoff-mcp-1","source_event_ids":[report.id],
        "source_actor_id":"worker","target_actor_id":"web-lead",
        "turn_id":"turn-mcp-1","summary":"Implementation ready; release window needed.",
        "task_ids":[],"status":"pending_review"
    })
    .as_object()
    .cloned()
    .expect("handoff");
    ledger::append(&path, &handoff).expect("handoff");
    let mut claimed = Event::new("runtime.delivery", &group.group_id);
    claimed.by = "system".into();
    claimed.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "delivery_id":"delivery-mcp-1","state":"claimed","transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("claim");
    ledger::append(&path, &claimed).expect("claim");

    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = DaemonClient::new(home.clone());
    for _ in 0..100 {
        if client
            .call(&DaemonRequest {
                v: 1,
                op: "ping".into(),
                args: Map::new(),
            })
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let catalog = cccc_mcp::handle_request_for_actor(
        &home,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &group.group_id,
        "web-lead",
    )
    .await;
    let coordination = catalog["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "cccc_coordination")
        .expect("coordination tool");
    assert!(
        coordination["inputSchema"]["properties"]["action"]["enum"]
            .as_array()
            .expect("actions")
            .contains(&json!("decide"))
    );

    let bootstrap = cccc_mcp::handle_request_for_actor(
        &home,
        &request("cccc_bootstrap", json!({})),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(
        payload(&bootstrap)["relay_pending"]["count"],
        1,
        "{bootstrap}"
    );
    assert_eq!(
        payload(&bootstrap)["relay_pending"]["pending"][0]["source_event_ids"],
        json!([report.id])
    );

    let inbox = cccc_mcp::handle_request_for_actor(
        &home,
        &request("cccc_inbox_read", json!({})),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(payload(&inbox)["messages"][0]["id"], report.id);
    assert_eq!(payload(&inbox)["relay_pending"]["requires_decision"], true);
    assert_eq!(payload(&inbox)["relay_pending"]["safe_to_idle"], false);

    let reply = cccc_mcp::handle_request_for_actor(
        &home,
        &request(
            "cccc_message_reply",
            json!({
                "event_id":report.id,"text":"I have read this, but have not chosen the next responsibility yet.",
                "mode":"mail",
                "insight":"A visible acknowledgement and a durable responsibility decision are separate; this reply deliberately leaves the handoff unresolved."
            }),
        ),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(payload(&reply)["relay_pending"]["count"], 1, "{reply}");
    assert_eq!(payload(&reply)["relay_pending"]["requires_decision"], true);

    let after_read = cccc_mcp::handle_request_for_actor(
        &home,
        &request("cccc_coordination", json!({"action":"get"})),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(payload(&after_read)["relay_pending"]["count"], 1);

    let decision = cccc_mcp::handle_request_for_actor(
        &home,
        &request(
            "cccc_coordination",
            json!({
                "action":"decide","event_ids":[report.id],"decision":"wait_user"
            }),
        ),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(
        payload(&decision)["relay"]["decision"],
        "wait_user",
        "{decision}"
    );
    assert_eq!(payload(&decision)["relay"]["safe_to_idle"], true);
    assert_eq!(payload(&decision)["caller_may_idle"], true);

    let replay = cccc_mcp::handle_request_for_actor(
        &home,
        &request(
            "cccc_coordination",
            json!({
                "action":"decide","event_ids":[report.id],"decision":"wait_user",
                "summary":"Legacy wording is accepted but is not emitted as another message."
            }),
        ),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(payload(&replay)["replayed"], true);

    let final_bootstrap = cccc_mcp::handle_request_for_actor(
        &home,
        &request("cccc_bootstrap", json!({})),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(payload(&final_bootstrap)["relay_pending"]["count"], 0);
    let events = ledger::read_all(&path).expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "chat.message"
                    && event.by == "web-lead"
                    && event.data["to"] == json!(["user"])
            })
            .count(),
        0,
        "machine-only decision duplicated the normal human-facing output"
    );
    assert_eq!(
        events
            .iter()
            .rev()
            .find(|event| {
                event.kind == "runtime.delivery"
                    && event.data["actor_id"] == "web-lead"
                    && event.data["source_event_id"] == report.id
            })
            .expect("latest delivery")
            .data["state"],
        "accepted"
    );

    let _ = client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Map::new(),
        })
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}

#[tokio::test]
async fn mcp_continue_creates_one_real_next_task_and_transfers_responsibility() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("relay MCP continue", "").expect("group");
    let mut lead = Actor::new("web-lead");
    lead.runtime = ActorRuntime::WebModel;
    actors::add(&mut group, lead).expect("lead");
    actors::add(&mut group, Actor::new("worker-a")).expect("worker a");
    actors::add(&mut group, Actor::new("worker-b")).expect("worker b");
    group.running = true;
    store.save(&group).expect("save group");
    let path = store.ledger_path(&group.group_id).expect("ledger path");

    let mut report = Event::new("chat.message", &group.group_id);
    report.by = "worker-a".into();
    report.data = json!({
        "to":["web-lead"],"message_mode":"mail",
        "text":"Implementation is complete. Run the full affected regression next."
    })
    .as_object()
    .cloned()
    .expect("report");
    ledger::append(&path, &report).expect("report");
    let mut handoff = Event::new("coordination.handoff", &group.group_id);
    handoff.by = "worker-a".into();
    handoff.data = json!({
        "handoff_id":"handoff-mcp-continue","source_event_ids":[report.id],
        "source_actor_id":"worker-a","target_actor_id":"web-lead",
        "turn_id":"turn-mcp-continue","summary":"Implementation complete; regression pending.",
        "task_ids":[],"status":"pending_review"
    })
    .as_object()
    .cloned()
    .expect("handoff");
    ledger::append(&path, &handoff).expect("handoff");
    let mut claimed = Event::new("runtime.delivery", &group.group_id);
    claimed.by = "system".into();
    claimed.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "delivery_id":"delivery-mcp-continue","state":"claimed",
        "transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("claim");
    ledger::append(&path, &claimed).expect("claim");

    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = DaemonClient::new(home.clone());
    for _ in 0..100 {
        if client
            .call(&DaemonRequest {
                v: 1,
                op: "ping".into(),
                args: Map::new(),
            })
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let args = json!({
        "action":"decide","event_ids":[report.id],"decision":"continue",
        "next_actor_id":"worker-b","next_title":"Run the affected regression",
        "next_text":"Run every affected Rust and browser-delivery test. Report the exact commands, failures, remaining risk, and requested next action.",
        "outcome":"All affected checks pass with evidence"
    });
    let decision = cccc_mcp::handle_request_for_actor(
        &home,
        &request("cccc_coordination", args.clone()),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(
        payload(&decision)["relay"]["decision"],
        "continue",
        "{decision}"
    );
    assert_eq!(payload(&decision)["caller_may_idle"], true);
    assert_eq!(payload(&decision)["safe_to_idle"], false);
    assert_eq!(
        payload(&decision)["current_responsibility"]["kind"],
        "actor_work"
    );
    assert_eq!(
        payload(&decision)["current_responsibility"]["tasks"][0]["actor_id"],
        "worker-b"
    );
    let next_task_id = payload(&decision)["relay"]["next_task_id"]
        .as_str()
        .expect("next task id")
        .to_owned();

    let replay = cccc_mcp::handle_request_for_actor(
        &home,
        &request("cccc_coordination", args),
        &group.group_id,
        "web-lead",
    )
    .await;
    assert_eq!(payload(&replay)["replayed"], true);
    assert_eq!(payload(&replay)["relay"]["next_task_id"], next_task_id);

    let context = cccc_core::context::ContextStore::new(home.clone())
        .expect("context")
        .load(&group.group_id)
        .expect("context document");
    let next_task = context
        .tasks
        .iter()
        .find(|task| task.get("id").and_then(Value::as_str) == Some(next_task_id.as_str()))
        .expect("next task");
    assert_eq!(next_task["status"], "active");
    assert_eq!(next_task["assignee"], "worker-b");
    assert_eq!(next_task["waiting_on"], "actor");

    let events = ledger::read_all(&path).expect("events");
    let next_messages = events
        .iter()
        .filter(|event| {
            event.kind == "chat.message"
                && event.by == "web-lead"
                && event.data["to"] == json!(["worker-b"])
        })
        .collect::<Vec<_>>();
    assert_eq!(
        next_messages.len(),
        1,
        "continue replay duplicated real work"
    );
    assert_eq!(
        next_messages[0].data["text"],
        "Run every affected Rust and browser-delivery test. Report the exact commands, failures, remaining risk, and requested next action."
    );
    assert_eq!(
        events
            .iter()
            .rev()
            .find(|event| {
                event.kind == "runtime.delivery"
                    && event.data["actor_id"] == "web-lead"
                    && event.data["source_event_id"] == report.id
            })
            .expect("latest source delivery")
            .data["state"],
        "accepted"
    );

    let _ = client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Map::new(),
        })
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}
