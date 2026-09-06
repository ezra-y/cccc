use super::*;
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event};
use cccc_core::context::{ContextDoc, ContextStore};
use cccc_core::{GroupStore, HomeLayout, actors, inbox, ledger};
use chrono::{Duration, Utc};
use serde_json::{Map, Value, json};

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
}

impl Fixture {
    fn new(title: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("group store");
        let mut group = store.create(title, "").expect("group");
        let mut lead = Actor::new("web-lead");
        lead.runtime = ActorRuntime::WebModel;
        actors::add(&mut group, lead).expect("web lead");
        actors::add(&mut group, Actor::new("worker-a")).expect("worker a");
        actors::add(&mut group, Actor::new("worker-b")).expect("worker b");
        group.running = true;
        store.save(&group).expect("save group");
        Self {
            _temp: temp,
            home,
            group,
        }
    }

    fn path(&self) -> std::path::PathBuf {
        GroupStore::new(self.home.clone())
            .expect("store")
            .ledger_path(&self.group.group_id)
            .expect("ledger path")
    }

    fn context(&self) -> ContextDoc {
        ContextStore::new(self.home.clone())
            .expect("context store")
            .load(&self.group.group_id)
            .expect("context")
    }

    fn task(&self, title: &str, assignee: &str) -> String {
        let result = ContextStore::new(self.home.clone())
            .expect("context store")
            .sync(
                &self.group.group_id,
                &[json!({
                    "op":"task.create","title":title,"outcome":format!("Finish {title}"),
                    "status":"active","assignee":assignee,"waiting_on":"actor"
                })
                .as_object()
                .cloned()
                .expect("task op")],
                None,
                "web-lead",
                false,
            )
            .expect("create task");
        result.context.tasks.last().expect("task")["id"]
            .as_str()
            .expect("task id")
            .to_owned()
    }

    fn report(&self, actor_id: &str, text: &str, task_id: Option<&str>) -> Event {
        let mut refs = Vec::new();
        if let Some(task_id) = task_id {
            refs.push(json!({
                "kind":"task_ref","task_id":task_id,"title":"Source task",
                "status":"active","waiting_on":"actor"
            }));
        }
        let mut report = Event::new("chat.message", &self.group.group_id);
        report.by = actor_id.into();
        report.data = json!({
            "to":["web-lead"],"message_mode":"mail","text":text,"refs":refs
        })
        .as_object()
        .cloned()
        .expect("report data");
        ledger::append(&self.path(), &report).expect("append report");
        report
    }

    fn handoff(&self, report: &Event, turn_id: &str) -> Event {
        record_handoff(
            &self.home,
            &self.group,
            &report.by,
            "web-lead",
            turn_id,
            std::slice::from_ref(report),
            "completed",
        )
        .expect("record handoff")
    }

    fn claim_for_web(&self, event: &Event) {
        let lead = actors::find(&self.group, "web-lead").expect("lead");
        super::super::runtime_delivery::append_state(
            &self.home,
            &self.group.group_id,
            &lead.id,
            &lead.created_at,
            &event.id,
            "web_model_browser",
            super::super::runtime_delivery::DeliveryOutcome::Claimed,
        )
        .expect("claim source");
    }

    fn decide(&self, value: Value) -> Result<Map<String, Value>, OpError> {
        let mut args = value.as_object().cloned().expect("decision args");
        args.insert("group_id".into(), json!(self.group.group_id));
        args.insert("by".into(), json!("web-lead"));
        decide(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: "coordination_decide".into(),
                args,
            },
        )
    }

    fn events(&self) -> Vec<Event> {
        ledger::read_all(&self.path()).expect("ledger")
    }
}

fn task<'a>(context: &'a ContextDoc, task_id: &str) -> &'a Map<String, Value> {
    context
        .tasks
        .iter()
        .find(|task| task.get("id").and_then(Value::as_str) == Some(task_id))
        .expect("task exists")
}

fn relay_events<'a>(events: &'a [Event], kind: &str) -> Vec<&'a Event> {
    events.iter().filter(|event| event.kind == kind).collect()
}

#[test]
fn continue_creates_real_work_resolves_the_report_and_replays_without_duplicates() {
    let fixture = Fixture::new("relay continue");
    let source_task = fixture.task("Implement relay", "worker-a");
    let report = fixture.report(
        "worker-a",
        "Implemented the relay and verified the focused tests. Review the diff before merging.",
        Some(&source_task),
    );
    fixture.handoff(&report, "turn-continue");
    fixture.claim_for_web(&report);
    assert_eq!(
        super::super::runtime_delivery::pending_sources(
            &fixture.home,
            &fixture.group,
            actors::find(&fixture.group, "web-lead").expect("lead"),
            20,
        )
        .expect("pending before decision")
        .len(),
        1
    );

    let request = json!({
        "event_ids":[report.id],"decision":"continue",
        "next_actor_id":"worker-b","next_title":"Run the complete regression",
        "next_text":"Run all affected Rust tests and report any regression with exact evidence.",
        "outcome":"All affected tests pass with recorded evidence"
    });
    let first = fixture.decide(request.clone()).expect("continue decision");
    assert_eq!(first["relay"]["decision"], "continue");
    assert_eq!(first["caller_may_idle"], true);
    assert_eq!(first["relay"]["safe_to_idle"], false);
    assert_eq!(first["safe_to_idle"], false);
    assert_eq!(first["current_responsibility"]["kind"], "actor_work");
    let next_task = first["relay"]["next_task_id"]
        .as_str()
        .expect("next task id")
        .to_owned();

    let context = fixture.context();
    assert_eq!(task(&context, &source_task)["status"], "done");
    assert_eq!(task(&context, &next_task)["status"], "active");
    assert_eq!(task(&context, &next_task)["assignee"], "worker-b");
    assert_eq!(task(&context, &next_task)["waiting_on"], "actor");
    let events = fixture.events();
    assert_eq!(relay_events(&events, DECISION_KIND).len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "chat.message"
                    && event.by == "web-lead"
                    && event.data["to"] == json!(["worker-b"])
            })
            .count(),
        1,
        "continue must have one visible task message"
    );
    let delegated = events
        .iter()
        .find(|event| {
            event.kind == "chat.message"
                && event.by == "web-lead"
                && event.data["to"] == json!(["worker-b"])
        })
        .expect("delegated message");
    assert_eq!(
        delegated.data["text"],
        "Run all affected Rust tests and report any regression with exact evidence."
    );
    assert!(
        !delegated.data["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Previous handoff reviewed")
    );
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &fixture.home,
            &fixture.group.group_id,
            "web-lead",
            &report.id,
        )
        .expect("delivery state")
        .expect("accepted state")
        .0,
        "accepted"
    );
    assert!(
        super::super::runtime_delivery::pending_sources(
            &fixture.home,
            &fixture.group,
            actors::find(&fixture.group, "web-lead").expect("lead"),
            20,
        )
        .expect("pending after decision")
        .is_empty(),
        "handled report was still eligible for browser wake"
    );

    let replay = fixture.decide(request).expect("idempotent replay");
    assert_eq!(replay["replayed"], true);
    let context = fixture.context();
    assert_eq!(context.tasks.len(), 2, "replay created another next task");
    let events = fixture.events();
    assert_eq!(relay_events(&events, DECISION_KIND).len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "chat.message"
                    && event.by == "web-lead"
                    && event.data["to"] == json!(["worker-b"])
            })
            .count(),
        1,
        "replay duplicated the visible task message"
    );

    let conflict = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user"
        }))
        .expect_err("conflicting decision");
    assert_eq!(conflict.code, "relay_decision_conflict");
}

#[test]
fn wait_user_records_machine_state_without_duplicating_the_original_output() {
    let fixture = Fixture::new("relay wait user");
    let source_task = fixture.task("Inspect interaction", "worker-a");
    let report = fixture.report(
        "worker-a",
        "The implementation is ready, but the user must choose between the two interaction variants.",
        Some(&source_task),
    );
    fixture.handoff(&report, "turn-wait-user");
    let result = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user"
        }))
        .expect("wait-user decision");
    assert_eq!(result["relay"]["decision"], "wait_user");
    assert_eq!(result["relay"]["summary"], "Waiting for user");
    assert_eq!(result["relay"]["responsibility"]["kind"], "user");
    assert_eq!(result["safe_to_idle"], true);

    let context = fixture.context();
    assert_eq!(task(&context, &source_task)["status"], "active");
    assert_eq!(task(&context, &source_task)["waiting_on"], "user");
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "chat.message")
            .count(),
        1,
        "decide duplicated the original visible report"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "chat.message"
                    && event.by == "web-lead"
                    && event.data["to"] == json!(["user"])
            })
            .count(),
        0
    );
}

#[test]
fn task_updates_are_limited_to_the_handoff_source_and_unique_unreferenced_work_is_inferred() {
    let fixture = Fixture::new("relay task ownership");
    let source_task = fixture.task("Unreferenced source task", "worker-a");
    let unrelated_task = fixture.task("Unrelated task", "worker-b");
    let report = fixture.report("worker-a", "Source task finished.", None);
    fixture.handoff(&report, "turn-owned-task");

    let error = fixture
        .decide(json!({
            "event_ids":[report.id],"task_id":unrelated_task,"decision":"wait_user",
            "summary":"Do not mutate this unrelated task."
        }))
        .expect_err("unrelated task rejected");
    assert_eq!(error.code, "relay_task_not_owned");
    assert_eq!(
        task(&fixture.context(), &unrelated_task)["status"],
        "active"
    );

    let result = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"complete",
            "summary":"The one source-owned task is complete."
        }))
        .expect_err("unrelated live task still blocks group completion");
    assert_eq!(result.code, "relay_work_remains");
    assert_eq!(task(&fixture.context(), &source_task)["status"], "active");

    ContextStore::new(fixture.home.clone())
        .expect("context")
        .sync(
            &fixture.group.group_id,
            &[
                json!({"op":"task.move","task_id":unrelated_task,"status":"done"})
                    .as_object()
                    .cloned()
                    .expect("task move"),
            ],
            None,
            "worker-b",
            false,
        )
        .expect("finish unrelated task");
    let result = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"complete",
            "summary":"The one source-owned task is complete."
        }))
        .expect("complete inferred task");
    assert_eq!(result["relay"]["task_ids"], json!([source_task]));
    assert_eq!(task(&fixture.context(), &source_task)["status"], "done");
}

#[test]
fn replay_recomputes_group_safety_after_new_work_appears() {
    let fixture = Fixture::new("replay current safety");
    let report = fixture.report("worker-a", "Please ask the user.", None);
    fixture.handoff(&report, "turn-replay-safety");
    let request = json!({
        "event_ids":[report.id],"decision":"wait_user",
        "summary":"Please confirm the next direction."
    });
    let first = fixture.decide(request.clone()).expect("first decision");
    assert_eq!(first["safe_to_idle"], true);
    fixture.task("New actor-owned work", "worker-b");
    let replay = fixture.decide(request).expect("replay");
    assert_eq!(replay["replayed"], true);
    assert_eq!(
        replay["relay"]["safe_to_idle"], true,
        "historical fact changed"
    );
    assert_eq!(
        replay["safe_to_idle"], false,
        "current responsibility stayed stale"
    );
    assert_eq!(replay["current_responsibility"]["kind"], "actor_work");
}

#[test]
fn complete_refuses_to_hide_other_live_work_then_succeeds_after_it_is_done() {
    let fixture = Fixture::new("relay complete");
    let source_task = fixture.task("Primary task", "worker-a");
    let other_task = fixture.task("Still unfinished", "worker-b");
    let report = fixture.report(
        "worker-a",
        "Primary task is complete and tested.",
        Some(&source_task),
    );
    fixture.handoff(&report, "turn-complete");

    let request = json!({
        "event_ids":[report.id],"decision":"complete",
        "summary":"The requested work is complete and verified."
    });
    let blocked = fixture
        .decide(request.clone())
        .expect_err("other live work must block completion");
    assert_eq!(blocked.code, "relay_work_remains");
    let context = fixture.context();
    assert_eq!(task(&context, &source_task)["status"], "active");
    assert_eq!(task(&context, &other_task)["status"], "active");
    assert!(relay_events(&fixture.events(), DECISION_KIND).is_empty());
    assert!(
        !fixture.events().iter().any(|event| {
            event.kind == "chat.message"
                && event.by == "web-lead"
                && event.data["text"] == "The requested work is complete and verified."
        }),
        "rejected complete was shown to the user"
    );

    ContextStore::new(fixture.home.clone())
        .expect("context")
        .sync(
            &fixture.group.group_id,
            &[
                json!({"op":"task.move","task_id":other_task,"status":"done"})
                    .as_object()
                    .cloned()
                    .expect("task move"),
            ],
            None,
            "worker-b",
            false,
        )
        .expect("finish other task");
    let completed = fixture.decide(request).expect("complete decision");
    assert_eq!(completed["relay"]["decision"], "complete");
    assert_eq!(completed["relay"]["safe_to_idle"], true);
    assert_eq!(task(&fixture.context(), &source_task)["status"], "done");
}

#[test]
fn relay_status_reports_current_group_responsibility_not_only_pending_handoffs() {
    let mut fixture = Fixture::new("current responsibility");
    let task_id = fixture.task("Actor still owns work", "worker-a");
    let active = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({"group_id":fixture.group.group_id,"actor_id":"web-lead"})
                .as_object()
                .cloned()
                .expect("args"),
        },
    )
    .expect("active state");
    assert_eq!(active["count"], 0);
    assert_eq!(active["safe_to_idle"], false);
    assert_eq!(active["responsibility"]["kind"], "actor_work");
    assert_eq!(active["responsibility"]["tasks"][0]["task_id"], task_id);

    fixture.group.state = cccc_contracts::GroupState::Paused;
    GroupStore::new(fixture.home.clone())
        .expect("store")
        .save(&fixture.group)
        .expect("pause");
    let paused = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({"group_id":fixture.group.group_id,"actor_id":"web-lead"})
                .as_object()
                .cloned()
                .expect("args"),
        },
    )
    .expect("paused state");
    assert_eq!(paused["safe_to_idle"], true);
    assert_eq!(paused["responsibility"]["kind"], "user_pause");
}

#[test]
fn user_pause_blocks_continue_and_automatic_reminders_without_losing_the_handoff() {
    let mut fixture = Fixture::new("paused relay");
    let report = fixture.report("worker-a", "Ready for the next task.", None);
    fixture.handoff(&report, "turn-paused");
    fixture.group.state = cccc_contracts::GroupState::Paused;
    GroupStore::new(fixture.home.clone())
        .expect("store")
        .save(&fixture.group)
        .expect("pause group");
    let error = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"continue","summary":"Continue",
            "next_actor_id":"worker-b","next_title":"Next","next_text":"Do the next step"
        }))
        .expect_err("paused group cannot continue");
    assert_eq!(error.code, "relay_group_paused");
    assert_eq!(fixture.context().tasks.len(), 0);
    assert!(relay_events(&fixture.events(), DECISION_KIND).is_empty());

    let mut delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    delivery.ts = (Utc::now() - Duration::seconds(30)).to_rfc3339();
    delivery.by = "system".into();
    delivery.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "state":"accepted","transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("delivery");
    ledger::append(&fixture.path(), &delivery).expect("delivery");
    let reminder = remind_due(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_remind".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("args"),
        },
    )
    .expect("paused reminder check");
    assert_eq!(reminder["reminded"], false);
    assert_eq!(reminder["reason"], "actor_inactive");
    assert_eq!(
        status(
            &fixture.home,
            &DaemonRequest {
                v: 1,
                op: "coordination_relay_status".into(),
                args: json!({"group_id":fixture.group.group_id,"actor_id":"web-lead"})
                    .as_object()
                    .cloned()
                    .expect("status args"),
            },
        )
        .expect("status")["count"],
        1,
        "pause discarded the handoff"
    );
}

#[test]
fn blocked_requires_a_reason_and_waiting_state_remains_visible() {
    let fixture = Fixture::new("relay blocked");
    let source_task = fixture.task("External integration", "worker-a");
    let report = fixture.report(
        "worker-a",
        "The provider endpoint rejects the required request.",
        Some(&source_task),
    );
    fixture.handoff(&report, "turn-blocked");
    let missing = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"blocked",
            "summary":"The external provider is blocking progress."
        }))
        .expect_err("blocked reason required");
    assert_eq!(missing.code, "relay_reason_required");

    let result = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"blocked",
            "summary":"The external provider is blocking progress.",
            "reason":"The provider returns HTTP 503 for the required endpoint."
        }))
        .expect("blocked decision");
    assert_eq!(result["relay"]["safe_to_idle"], true);
    assert_eq!(result["relay"]["responsibility"]["kind"], "external");
    let context = fixture.context();
    assert_eq!(task(&context, &source_task)["waiting_on"], "external");
    assert_eq!(
        task(&context, &source_task)["notes"],
        "The provider returns HTTP 503 for the required endpoint."
    );
}

#[test]
fn reading_mail_is_not_acknowledgement_but_deciding_cancels_later_browser_wake() {
    let fixture = Fixture::new("read then decide");
    let task_id = fixture.task("Review report", "worker-a");
    let report = fixture.report(
        "worker-a",
        "Read this report through CCCC while the web model is still answering.",
        Some(&task_id),
    );
    fixture.handoff(&report, "turn-read");
    fixture.claim_for_web(&report);
    let wait = super::super::runtime_state::handle(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "runtime_wait_next_turn".into(),
            args: json!({
                "group_id":fixture.group.group_id,
                "actor_id":"web-lead","by":"web-lead","transport":"web_model_browser"
            })
            .as_object()
            .cloned()
            .expect("wait request"),
        },
    )
    .expect("wait operation")
    .expect("work available");
    assert_eq!(wait["status"], "work_available");
    assert_eq!(
        super::super::runtime_state::actor_state(
            &fixture.home,
            &fixture.group.group_id,
            "web-lead",
        )
        .expect("active state")["status"],
        "working"
    );

    let consumed = inbox::consume_unread(&fixture.home, &fixture.group, "web-lead", "web-lead", 20)
        .expect("read mail");
    assert_eq!(
        consumed
            .messages
            .iter()
            .map(|event| &event.id)
            .collect::<Vec<_>>(),
        [&report.id]
    );
    assert_eq!(consumed.read_event.expect("read event").kind, "mail.read");
    assert_eq!(
        super::super::runtime_delivery::pending_sources(
            &fixture.home,
            &fixture.group,
            actors::find(&fixture.group, "web-lead").expect("lead"),
            20,
        )
        .expect("pending after read")
        .iter()
        .map(|event| &event.id)
        .collect::<Vec<_>>(),
        [&report.id],
        "read was incorrectly treated as acknowledgement"
    );

    fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"I reviewed the report inside the current turn. Please confirm the release window."
        }))
        .expect("decision after read");
    assert!(
        super::super::runtime_delivery::pending_sources(
            &fixture.home,
            &fixture.group,
            actors::find(&fixture.group, "web-lead").expect("lead"),
            20,
        )
        .expect("pending after decision")
        .is_empty(),
        "explicit handling did not suppress the later browser wake"
    );
    assert_eq!(
        super::super::runtime_state::actor_state(
            &fixture.home,
            &fixture.group.group_id,
            "web-lead",
        )
        .expect("released state")["status"],
        "waiting",
        "the claimed browser turn remained stuck after in-turn handling"
    );
}

#[test]
fn status_reconciles_a_durable_decision_whose_transport_acceptance_was_interrupted() {
    let fixture = Fixture::new("decision acceptance recovery");
    let report = fixture.report("worker-a", "Reviewed output.", None);
    let handoff = fixture.handoff(&report, "turn-recovery");
    fixture.claim_for_web(&report);
    let decision_id = "decision-recovery";
    let mut decision = Event::new(DECISION_KIND, &fixture.group.group_id);
    decision.id = stable_event_id(decision_id);
    decision.by = "web-lead".into();
    decision.data = json!({
        "decision_id":decision_id,"by":"web-lead","decision":"wait_user",
        "summary":"Wait for the user's approval.","source_event_ids":[report.id],
        "handoff_ids":[handoff.data["handoff_id"]],"status":"applied",
        "caller_may_idle":true,"safe_to_idle":true
    })
    .as_object()
    .cloned()
    .expect("decision data");
    ledger::append(&fixture.path(), &decision).expect("durable decision");
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &fixture.home,
            &fixture.group.group_id,
            "web-lead",
            &report.id,
        )
        .expect("state")
        .expect("claim")
        .0,
        "claimed"
    );
    let result = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({"group_id":fixture.group.group_id,"actor_id":"web-lead"})
                .as_object()
                .cloned()
                .expect("status args"),
        },
    )
    .expect("self-healing status");
    assert_eq!(result["count"], 0);
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &fixture.home,
            &fixture.group.group_id,
            "web-lead",
            &report.id,
        )
        .expect("state")
        .expect("accepted")
        .0,
        "accepted"
    );
}

#[test]
fn delivered_unresolved_handoff_gets_one_reminder_and_decision_resolves_it_too() {
    let fixture = Fixture::new("relay reminder");
    let task_id = fixture.task("Review result", "worker-a");
    let report = fixture.report(
        "worker-a",
        "The result reached the web conversation but no decision was recorded.",
        Some(&task_id),
    );
    let handoff = fixture.handoff(&report, "turn-reminder");
    let mut delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    delivery.ts = (Utc::now() - Duration::seconds(30)).to_rfc3339();
    delivery.by = "system".into();
    delivery.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "delivery_id":"delivery-old","state":"accepted","transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("delivery");
    ledger::append(&fixture.path(), &delivery).expect("old delivery");

    let reminder_request = DaemonRequest {
        v: 1,
        op: "coordination_relay_remind".into(),
        args: json!({
            "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
        })
        .as_object()
        .cloned()
        .expect("reminder request"),
    };
    let first = remind_due(&fixture.home, &reminder_request).expect("first reminder");
    assert_eq!(first["reminded"], true);
    let reminder_id = first["reminder_event"]["id"]
        .as_str()
        .expect("reminder id")
        .to_owned();
    assert!(
        first["reminder_event"]["data"]["text"]
            .as_str()
            .expect("reminder text")
            .contains("call cccc_coordination")
    );
    let second = remind_due(&fixture.home, &reminder_request).expect("second reminder check");
    assert_eq!(second["reminded"], false);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event.data.get("relay_kind").and_then(Value::as_str)
                == Some("decision_reminder"))
            .count(),
        1
    );

    fixture
        .decide(json!({
            "event_ids":[reminder_id],"decision":"wait_user",
            "summary":"Please approve the final rollout."
        }))
        .expect("resolve reminder");
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &fixture.home,
            &fixture.group.group_id,
            "web-lead",
            &reminder_id,
        )
        .expect("reminder delivery")
        .expect("reminder accepted")
        .0,
        "accepted"
    );
    let handoff_id = handoff.data["handoff_id"].as_str().expect("handoff id");
    let context = fixture.context();
    let note = context.coordination["recent_handoffs"]
        .as_array()
        .expect("handoffs")
        .iter()
        .find(|note| note["id"] == handoff_id)
        .expect("resolved handoff note");
    assert_eq!(note["status"], "resolved");
}

#[test]
fn repeated_completion_after_a_decision_never_reopens_the_resolved_handoff() {
    let fixture = Fixture::new("resolved handoff replay");
    let report = fixture.report("worker-a", "Final human-readable result.", None);
    fixture.handoff(&report, "turn-resolved-replay");
    fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Please approve the verified result."
        }))
        .expect("decision");
    let version_before = ContextStore::new(fixture.home.clone())
        .expect("context")
        .version(&fixture.context())
        .expect("version");
    fixture.handoff(&report, "turn-resolved-replay");
    let context = fixture.context();
    let version_after = ContextStore::new(fixture.home.clone())
        .expect("context")
        .version(&context)
        .expect("version");
    assert_eq!(
        version_before, version_after,
        "duplicate completion rewrote context"
    );
    let notes = context.coordination["recent_handoffs"]
        .as_array()
        .expect("handoffs");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["status"], "resolved");
    assert!(
        notes[0]["decision_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
}

#[test]
fn one_visible_output_resolves_every_message_in_the_same_member_handoff() {
    let fixture = Fixture::new("multi-output decision");
    let first = fixture.report("worker-a", "First human-readable result.", None);
    let second = fixture.report("worker-a", "Second human-readable result.", None);
    record_handoff(
        &fixture.home,
        &fixture.group,
        "worker-a",
        "web-lead",
        "turn-multi-output",
        &[first.clone(), second.clone()],
        "completed",
    )
    .expect("multi-output handoff");
    fixture.claim_for_web(&first);
    fixture.claim_for_web(&second);

    let result = fixture
        .decide(json!({
            "event_id":first.id,"decision":"wait_user",
            "summary":"Both visible outputs were reviewed. Please choose the next direction."
        }))
        .expect("complete handoff decision");
    let mut expected_ids = vec![first.id.clone(), second.id.clone()];
    expected_ids.sort();
    assert_eq!(result["relay"]["source_event_ids"], json!(expected_ids));
    for event_id in [&first.id, &second.id] {
        assert_eq!(
            super::super::runtime_delivery::latest_state(
                &fixture.home,
                &fixture.group.group_id,
                "web-lead",
                event_id,
            )
            .expect("delivery")
            .expect("accepted")
            .0,
            "accepted"
        );
    }
}

#[test]
fn legacy_member_report_can_be_decided_without_a_preexisting_machine_handoff() {
    let fixture = Fixture::new("implicit handoff");
    let report = fixture.report("worker-a", "Legacy human-readable report.", None);
    let result = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Please confirm the next requirement."
        }))
        .expect("implicit handoff decision");
    assert_eq!(result["relay"]["decision"], "wait_user");
    let events = fixture.events();
    assert_eq!(relay_events(&events, HANDOFF_KIND).len(), 1);
    assert_eq!(relay_events(&events, DECISION_KIND).len(), 1);
}

#[test]
fn only_the_foreman_may_decide_and_continue_requires_concrete_work() {
    let fixture = Fixture::new("relay permissions");
    let report = fixture.report("worker-a", "Ready for review.", None);
    fixture.handoff(&report, "turn-permission");
    let mut args = json!({
        "group_id":fixture.group.group_id,"by":"worker-a",
        "event_ids":[report.id],"decision":"wait_user","summary":"Need user input"
    })
    .as_object()
    .cloned()
    .expect("args");
    let forbidden = decide(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_decide".into(),
            args: std::mem::take(&mut args),
        },
    )
    .expect_err("peer decision forbidden");
    assert_eq!(forbidden.code, "relay_decision_forbidden");

    let incomplete = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"continue",
            "summary":"Continue the work"
        }))
        .expect_err("bare continue forbidden");
    assert_eq!(incomplete.code, "invalid_args");
    assert!(relay_events(&fixture.events(), DECISION_KIND).is_empty());
}

#[test]
fn decide_reuses_the_original_output_and_ignores_legacy_summary_wording() {
    let fixture = Fixture::new("relay original output");
    let report = fixture.report("worker-a", "A visible report already exists.", None);
    fixture.handoff(&report, "turn-original-output");
    let first = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user"
        }))
        .expect("summary is not required");
    assert_eq!(first["relay"]["summary"], "Waiting for user");
    let replay = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Legacy callers may still send wording, but it is not a second output."
        }))
        .expect("legacy summary wording must not change machine intent");
    assert_eq!(replay["replayed"], true);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event.kind == "chat.message")
            .count(),
        1
    );
}

#[test]
fn a_partially_dispatched_continue_cannot_be_changed_to_wait_user() {
    let fixture = Fixture::new("continue decision intent");
    let report = fixture.report(
        "worker-a",
        "The implementation needs one more verification pass.",
        None,
    );
    let handoff = fixture.handoff(&report, "turn-continue-partial");
    let source_event_ids = vec![report.id.clone()];
    let handoff_ids = vec![
        handoff.data["handoff_id"]
            .as_str()
            .expect("handoff id")
            .to_owned(),
    ];
    let decision_id = decision_id(&fixture.group.group_id, "web-lead", &source_event_ids);
    let request = DaemonRequest {
        v: 1,
        op: "coordination_decide".into(),
        args: json!({
            "decision":"continue","summary":"Run the final verification.",
            "next_actor_id":"worker-b","next_title":"Final verification",
            "next_text":"Run the full affected regression and report exact evidence."
        })
        .as_object()
        .cloned()
        .expect("request"),
    };
    let fingerprint = decision_fingerprint(
        "continue",
        "",
        "worker-b",
        "Final verification",
        "Run the full affected regression and report exact evidence.",
        &request,
    );
    let scope = DecisionScope {
        home: &fixture.home,
        group_id: &fixture.group.group_id,
        actor_id: "web-lead",
        decision_id: &decision_id,
        request_fingerprint: &fingerprint,
        source_event_ids: &source_event_ids,
        handoff_ids: &handoff_ids,
    };
    let sent = tracked_continue(
        &scope,
        &request,
        ContinueSpec {
            actor_id: "worker-b",
            title: "Final verification",
            text: "Run the full affected regression and report exact evidence.",
        },
    )
    .expect("simulate crash after next work was sent");
    assert_eq!(sent["message_sent"], true);

    let conflict = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Ask the user instead."
        }))
        .expect_err("real next work must not be contradicted by a later wait decision");
    assert_eq!(conflict.code, "relay_decision_conflict");
    assert!(relay_events(&fixture.events(), DECISION_KIND).is_empty());
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event.kind == "chat.message" && event.data["to"] == json!(["worker-b"]))
            .count(),
        1
    );
}

#[test]
fn an_applied_decision_replay_rejects_changed_machine_intent() {
    let fixture = Fixture::new("exact replay intent");
    let report = fixture.report("worker-a", "The provider is unavailable.", None);
    fixture.handoff(&report, "turn-exact-replay");
    fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"blocked",
            "reason":"The provider returns HTTP 503."
        }))
        .expect("first decision");
    let conflict = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"blocked",
            "reason":"The credential is invalid."
        }))
        .expect_err("a different blocking cause is different machine intent");
    assert_eq!(conflict.code, "relay_decision_conflict");
    assert_eq!(relay_events(&fixture.events(), DECISION_KIND).len(), 1);
}

#[test]
fn the_same_partially_dispatched_continue_recovers_without_duplicate_work() {
    let fixture = Fixture::new("continue decision recovery");
    let report = fixture.report(
        "worker-a",
        "The implementation needs one final verification.",
        None,
    );
    let handoff = fixture.handoff(&report, "turn-continue-recovery");
    let source_event_ids = vec![report.id.clone()];
    let handoff_ids = vec![
        handoff.data["handoff_id"]
            .as_str()
            .expect("handoff id")
            .to_owned(),
    ];
    let decision_id = decision_id(&fixture.group.group_id, "web-lead", &source_event_ids);
    let request = DaemonRequest {
        v: 1,
        op: "coordination_decide".into(),
        args: json!({
            "decision":"continue","summary":"Run the final verification.",
            "next_actor_id":"worker-b","next_title":"Final verification",
            "next_text":"Run the affected regression and report exact evidence."
        })
        .as_object()
        .cloned()
        .expect("request"),
    };
    let fingerprint = decision_fingerprint(
        "continue",
        "",
        "worker-b",
        "Final verification",
        "Run the affected regression and report exact evidence.",
        &request,
    );
    let scope = DecisionScope {
        home: &fixture.home,
        group_id: &fixture.group.group_id,
        actor_id: "web-lead",
        decision_id: &decision_id,
        request_fingerprint: &fingerprint,
        source_event_ids: &source_event_ids,
        handoff_ids: &handoff_ids,
    };
    tracked_continue(
        &scope,
        &request,
        ContinueSpec {
            actor_id: "worker-b",
            title: "Final verification",
            text: "Run the affected regression and report exact evidence.",
        },
    )
    .expect("simulate interrupted continue");
    let result = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"continue",
            "summary":"Run the final verification.",
            "next_actor_id":"worker-b","next_title":"Final verification",
            "next_text":"Run the affected regression and report exact evidence."
        }))
        .expect("same continue recovers");
    assert_eq!(result["relay"]["decision"], "continue");
    let context = fixture.context();
    let relay_tasks = context
        .tasks
        .iter()
        .filter(|task| {
            task.get("client_request_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.contains("tracked-send:"))
        })
        .count();
    assert_eq!(relay_tasks, 1);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event.kind == "chat.message" && event.data["to"] == json!(["worker-b"]))
            .count(),
        1
    );
}

#[test]
fn a_task_only_partial_continue_blocks_a_different_decision() {
    let fixture = Fixture::new("task-only decision intent");
    let report = fixture.report("worker-a", "A follow-up task was prepared.", None);
    fixture.handoff(&report, "turn-task-only");
    let source_event_ids = vec![report.id.clone()];
    let decision_id = decision_id(&fixture.group.group_id, "web-lead", &source_event_ids);
    let client_id = super::super::message_idempotency::tracked_client_id(
        &fixture.group.group_id,
        "web-lead",
        &decision_id,
    );
    ContextStore::new(fixture.home.clone())
        .expect("context")
        .sync(
            &fixture.group.group_id,
            &[json!({
                "op":"task.create","title":"Prepared follow-up","outcome":"Finish it",
                "status":"active","assignee":"worker-b","waiting_on":"actor",
                "client_request_id":client_id
            })
            .as_object()
            .cloned()
            .expect("task operation")],
            None,
            "web-lead",
            false,
        )
        .expect("prepare task-only partial effect");
    let conflict = fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Ask the user instead."
        }))
        .expect_err("prepared next work must prevent a contradictory wait decision");
    assert_eq!(conflict.code, "relay_decision_conflict");
}

#[test]
fn generic_context_sync_cannot_write_private_relay_machine_state() {
    let fixture = Fixture::new("relay note boundary");
    let request = DaemonRequest {
        v: 1,
        op: "context_sync".into(),
        args: json!({
            "group_id":fixture.group.group_id,"by":"user",
            "ops":[{"op":"coordination.relay.note","kind":"decision","id":"forged",
                "summary":"Pretend complete","decision":"complete","safe_to_idle":true}]
        })
        .as_object()
        .cloned()
        .expect("request"),
    };
    let error = super::super::context::handle(&fixture.home, &request)
        .expect("context operation")
        .expect_err("private relay state must not be public context input");
    assert_eq!(error.code, "permission_denied");
}

#[test]
fn resolving_one_handoff_does_not_tell_the_foreman_to_idle_with_another_pending() {
    let fixture = Fixture::new("multiple relay obligations");
    let first = fixture.report("worker-a", "First result needs a decision.", None);
    let second = fixture.report("worker-b", "Second result also needs a decision.", None);
    fixture.handoff(&first, "turn-first-obligation");
    fixture.handoff(&second, "turn-second-obligation");

    let first_result = fixture
        .decide(json!({
            "event_ids":[first.id],"decision":"wait_user",
            "summary":"The first result needs user input."
        }))
        .expect("first decision");
    assert_eq!(first_result["caller_may_idle"], false);
    assert_eq!(first_result["safe_to_idle"], false);
    assert_eq!(
        first_result["current_responsibility"]["kind"],
        "foreman_review"
    );

    let pending = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status request"),
        },
    )
    .expect("status");
    assert_eq!(pending["count"], 1);
    assert_eq!(pending["caller_may_idle"], false);

    let second_result = fixture
        .decide(json!({
            "event_ids":[second.id],"decision":"wait_user",
            "summary":"The second result also needs user input."
        }))
        .expect("second decision");
    assert_eq!(second_result["caller_may_idle"], true);
    assert_eq!(second_result["safe_to_idle"], true);
}

#[test]
fn unassigned_live_work_remains_the_foremans_triage_responsibility() {
    let fixture = Fixture::new("unassigned responsibility");
    ContextStore::new(fixture.home.clone())
        .expect("context")
        .sync(
            &fixture.group.group_id,
            &[json!({
                "op":"task.create","title":"Unassigned follow-up","outcome":"Find an owner",
                "status":"active","waiting_on":"actor"
            })
            .as_object()
            .cloned()
            .expect("task")],
            None,
            "web-lead",
            false,
        )
        .expect("create unassigned work");
    let state = current_group_state(&fixture.home, &fixture.group, &fixture.events())
        .expect("group responsibility");
    assert_eq!(state["safe_to_idle"], false);
    assert_eq!(state["responsibility"]["kind"], "actor_work");
    assert_eq!(state["responsibility"]["tasks"][0]["actor_id"], "web-lead");
    assert!(!actor_may_idle_from_state(&state, "web-lead"));
    assert!(actor_may_idle_from_state(&state, "worker-a"));
}

#[test]
fn repeated_status_reads_do_not_reappend_completed_delivery_facts() {
    let fixture = Fixture::new("status reconciliation cost");
    let report = fixture.report("worker-a", "The result needs user approval.", None);
    fixture.handoff(&report, "turn-status-stable");
    fixture.claim_for_web(&report);
    fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Please approve the verified result."
        }))
        .expect("decision");
    let accepted_before = fixture
        .events()
        .iter()
        .filter(|event| {
            event.kind == "runtime.delivery"
                && event.data["source_event_id"] == report.id
                && event.data["state"] == "accepted"
        })
        .count();
    assert_eq!(accepted_before, 1);
    for _ in 0..20 {
        status(
            &fixture.home,
            &DaemonRequest {
                v: 1,
                op: "coordination_relay_status".into(),
                args: json!({
                    "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
                })
                .as_object()
                .cloned()
                .expect("status"),
            },
        )
        .expect("stable status");
    }
    let accepted_after = fixture
        .events()
        .iter()
        .filter(|event| {
            event.kind == "runtime.delivery"
                && event.data["source_event_id"] == report.id
                && event.data["state"] == "accepted"
        })
        .count();
    assert_eq!(accepted_after, 1);
}

#[test]
fn an_in_turn_decision_does_not_reopen_the_same_report_at_member_completion() {
    let fixture = Fixture::new("early handled report");
    let report = fixture.report("worker-a", "The result is ready for user review.", None);
    fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Please review the completed result."
        }))
        .expect("decision before managed completion event");
    record_handoff(
        &fixture.home,
        &fixture.group,
        "worker-a",
        "web-lead",
        "real-managed-turn",
        std::slice::from_ref(&report),
        "completed",
    )
    .expect("late managed completion handoff");
    let state = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("status");
    assert_eq!(state["count"], 0, "late completion reopened handled output");
    assert_eq!(state["requires_decision"], false);
}

#[test]
fn a_new_output_after_an_early_decision_remains_a_separate_review_obligation() {
    let fixture = Fixture::new("partial early handling");
    let first = fixture.report("worker-a", "First result was reviewed early.", None);
    fixture
        .decide(json!({
            "event_ids":[first.id],"decision":"wait_user",
            "summary":"Please review the first result."
        }))
        .expect("early decision");
    let second = fixture.report(
        "worker-a",
        "A second result arrived before the turn ended.",
        None,
    );
    record_handoff(
        &fixture.home,
        &fixture.group,
        "worker-a",
        "web-lead",
        "real-multi-output-turn",
        &[first.clone(), second.clone()],
        "completed",
    )
    .expect("actual turn handoff");
    let pending = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("pending status");
    assert_eq!(pending["count"], 1);
    assert_eq!(
        pending["pending"][0]["source_event_ids"],
        json!([second.id])
    );
    assert_eq!(
        pending["pending"][0]["all_source_event_ids"],
        json!([first.id, second.id])
    );
    fixture
        .decide(json!({
            "event_ids":[second.id],"decision":"wait_user",
            "summary":"Please also review the second result."
        }))
        .expect("second output decision");
    let final_state = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("final status");
    assert_eq!(final_state["count"], 0);
}

#[test]
fn an_idle_foreman_that_ignores_one_reminder_escalates_once_to_the_user() {
    let fixture = Fixture::new("relay escalation");
    let report = fixture.report(
        "worker-a",
        "The finished work needs a durable next-step decision.",
        None,
    );
    let handoff = fixture.handoff(&report, "turn-escalation");
    let mut source_delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    source_delivery.ts = (Utc::now() - Duration::seconds(90)).to_rfc3339();
    source_delivery.by = "system".into();
    source_delivery.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "delivery_id":"delivery-escalation-source","state":"accepted",
        "transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("source delivery");
    ledger::append(&fixture.path(), &source_delivery).expect("source delivery");
    let request = |browser_idle| DaemonRequest {
        v: 1,
        op: "coordination_relay_remind".into(),
        args: json!({
            "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead",
            "browser_idle":browser_idle
        })
        .as_object()
        .cloned()
        .expect("reminder request"),
    };
    let reminded = remind_due(&fixture.home, &request(false)).expect("initial reminder");
    assert_eq!(reminded["reminded"], true);
    assert_eq!(reminded["escalated"], false);
    let reminder_id = reminded["reminder_event"]["id"]
        .as_str()
        .expect("reminder id")
        .to_owned();
    let mut reminder_delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    reminder_delivery.ts = (Utc::now() - Duration::seconds(40)).to_rfc3339();
    reminder_delivery.by = "system".into();
    reminder_delivery.data = json!({
        "actor_id":"web-lead","source_event_id":reminder_id,
        "delivery_id":"delivery-escalation-reminder","state":"accepted",
        "transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("reminder delivery");
    ledger::append(&fixture.path(), &reminder_delivery).expect("reminder delivery");

    let busy = remind_due(&fixture.home, &request(false)).expect("busy check");
    assert_eq!(busy["escalated"], false, "a working web page was escalated");
    let escalated = remind_due(&fixture.home, &request(true)).expect("idle escalation");
    assert_eq!(escalated["reminded"], false);
    assert_eq!(escalated["escalated"], true);
    let escalation_id = escalated["escalation_event"]["id"]
        .as_str()
        .expect("escalation id")
        .to_owned();
    assert_eq!(escalated["escalation_event"]["data"]["to"], json!(["user"]));
    assert!(
        escalated["escalation_event"]["data"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("No model will be woken repeatedly"))
    );

    let state = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("escalated status");
    assert_eq!(state["count"], 1);
    assert_eq!(state["requires_decision"], false);
    assert_eq!(state["awaiting_user_intervention"], true);
    assert_eq!(state["caller_may_idle"], true);
    assert_eq!(state["safe_to_idle"], true);
    assert_eq!(state["responsibility"]["kind"], "user_intervention");
    let handoff_id = handoff.data["handoff_id"].as_str().expect("handoff id");
    let note = fixture.context().coordination["recent_handoffs"]
        .as_array()
        .expect("handoff notes")
        .iter()
        .find(|note| note["id"] == handoff_id)
        .expect("escalated note")
        .clone();
    assert_eq!(note["status"], "waiting_user");
    assert_eq!(note["escalation_event_id"], escalation_id);
    assert!(
        super::super::runtime_delivery::pending_sources(
            &fixture.home,
            &fixture.group,
            actors::find(&fixture.group, "web-lead").expect("lead"),
            20,
        )
        .expect("pending after escalation")
        .is_empty(),
        "the user escalation re-entered the web model queue and could create a loop"
    );

    let repeated = remind_due(&fixture.home, &request(true)).expect("repeat escalation");
    assert_eq!(repeated["escalated"], false);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| {
                event.data.get("relay_kind").and_then(Value::as_str) == Some("decision_escalation")
            })
            .count(),
        1
    );

    fixture
        .decide(json!({
            "event_ids":[report.id],"decision":"wait_user",
            "summary":"Please choose the next step for the preserved result."
        }))
        .expect("explicit decision after user resumes the foreman");
    let resolved = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("resolved status");
    assert_eq!(resolved["count"], 0);
}

#[test]
fn user_escalation_does_not_hide_another_actors_live_work() {
    let fixture = Fixture::new("escalation plus live task");
    let live_task = fixture.task("Independent implementation", "worker-b");
    let report = fixture.report("worker-a", "A different result needs intervention.", None);
    let handoff = fixture.handoff(&report, "turn-escalation-with-live-work");
    let mut source_delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    source_delivery.ts = (Utc::now() - Duration::seconds(90)).to_rfc3339();
    source_delivery.by = "system".into();
    source_delivery.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "delivery_id":"delivery-live-source","state":"accepted","transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("source delivery");
    ledger::append(&fixture.path(), &source_delivery).expect("source delivery");
    let request = |browser_idle| DaemonRequest {
        v: 1,
        op: "coordination_relay_remind".into(),
        args: json!({
            "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead",
            "browser_idle":browser_idle
        })
        .as_object()
        .cloned()
        .expect("request"),
    };
    let reminder = remind_due(&fixture.home, &request(false)).expect("reminder");
    let reminder_id = reminder["reminder_event"]["id"]
        .as_str()
        .expect("reminder id")
        .to_owned();
    let mut reminder_delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    reminder_delivery.ts = (Utc::now() - Duration::seconds(40)).to_rfc3339();
    reminder_delivery.by = "system".into();
    reminder_delivery.data = json!({
        "actor_id":"web-lead","source_event_id":reminder_id,
        "delivery_id":"delivery-live-reminder","state":"accepted","transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("reminder delivery");
    ledger::append(&fixture.path(), &reminder_delivery).expect("reminder delivery");
    assert_eq!(
        remind_due(&fixture.home, &request(true)).expect("escalation")["escalated"],
        true
    );
    let state = current_group_state(&fixture.home, &fixture.group, &fixture.events())
        .expect("combined responsibility");
    assert_eq!(
        state["safe_to_idle"], false,
        "live work was hidden by user escalation"
    );
    assert_eq!(state["responsibility"]["kind"], "actor_work");
    assert_eq!(state["responsibility"]["tasks"][0]["task_id"], live_task);
    assert_eq!(
        state["responsibilities"]
            .as_array()
            .expect("responsibilities")
            .len(),
        2
    );
    assert!(!actor_may_idle_from_state(&state, "worker-b"));
    assert!(actor_may_idle_from_state(&state, "web-lead"));
    assert_eq!(
        state["user_intervention"]["handoff_ids"],
        json!([handoff.data["handoff_id"]])
    );
}

#[test]
fn foreman_review_does_not_hide_a_peers_independent_live_task() {
    let fixture = Fixture::new("review plus live peer task");
    let task_id = fixture.task("Independent peer work", "worker-b");
    let report = fixture.report("worker-a", "Review this completed result.", None);
    let handoff = fixture.handoff(&report, "turn-review-with-live-work");
    let state = current_group_state(&fixture.home, &fixture.group, &fixture.events())
        .expect("combined responsibility");
    assert_eq!(state["safe_to_idle"], false);
    assert_eq!(state["responsibility"]["kind"], "foreman_review");
    assert!(!actor_may_idle_from_state(&state, "web-lead"));
    assert!(!actor_may_idle_from_state(&state, "worker-b"));
    assert_eq!(state["actor_work"]["tasks"][0]["task_id"], task_id);
    assert_eq!(
        state["responsibilities"]
            .as_array()
            .expect("responsibilities")
            .len(),
        2
    );
    assert_eq!(
        state["responsibility"]["handoff_ids"],
        json!([handoff.data["handoff_id"]])
    );
}

#[test]
fn a_verbose_member_turn_keeps_all_human_outputs_inside_one_decidable_handoff() {
    let fixture = Fixture::new("verbose member handoff");
    let reports = (0..25)
        .map(|index| {
            fixture.report(
                "worker-a",
                &format!("Visible result part {index}: evidence and remaining risk."),
                None,
            )
        })
        .collect::<Vec<_>>();
    let handoff = record_handoff(
        &fixture.home,
        &fixture.group,
        "worker-a",
        "web-lead",
        "turn-verbose-output",
        &reports,
        "completed",
    )
    .expect("verbose handoff");
    assert_eq!(
        handoff.data["source_event_ids"]
            .as_array()
            .expect("source ids")
            .len(),
        25
    );
    fixture
        .decide(json!({
            "event_id":reports[0].id,"decision":"wait_user",
            "summary":"All 25 visible output parts were reviewed. Please choose the rollout window."
        }))
        .expect("one decision resolves the full member turn");
    let status = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("status");
    assert_eq!(status["count"], 0);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| {
                event.kind == "runtime.delivery"
                    && event.data["actor_id"] == "web-lead"
                    && event.data["state"] == "accepted"
            })
            .count(),
        25
    );
}

#[test]
fn status_repairs_the_context_note_after_an_escalation_write_is_interrupted() {
    let fixture = Fixture::new("escalation context recovery");
    let report = fixture.report("worker-a", "The result needs human intervention.", None);
    let handoff = fixture.handoff(&report, "turn-escalation-recovery");
    let handoff_id = handoff.data["handoff_id"].as_str().expect("handoff id");
    let mut escalation = Event::new("chat.message", &fixture.group.group_id);
    escalation.by = "system".into();
    escalation.data = json!({
        "to":["user"],"message_mode":"send","text":"Collaboration is waiting for your decision.",
        "relay_kind":"decision_escalation","relay_actor_id":"web-lead",
        "relay_handoff_ids":[handoff_id],"relay_source_event_ids":[report.id]
    })
    .as_object()
    .cloned()
    .expect("escalation");
    ledger::append(&fixture.path(), &escalation).expect("simulate visible escalation write");
    let before = fixture.context();
    assert_eq!(
        before.coordination["recent_handoffs"][0]["status"],
        "pending_review"
    );

    let result = status(
        &fixture.home,
        &DaemonRequest {
            v: 1,
            op: "coordination_relay_status".into(),
            args: json!({
                "group_id":fixture.group.group_id,"actor_id":"web-lead","by":"web-lead"
            })
            .as_object()
            .cloned()
            .expect("status"),
        },
    )
    .expect("status repairs context");
    assert_eq!(result["awaiting_user_intervention"], true);
    let after = fixture.context();
    let note = after.coordination["recent_handoffs"]
        .as_array()
        .expect("handoffs")
        .iter()
        .find(|note| note["id"] == handoff_id)
        .expect("repaired note");
    assert_eq!(note["status"], "waiting_user");
    assert_eq!(note["escalation_event_id"], escalation.id);
}

#[test]
fn concurrent_reminder_and_escalation_checks_create_one_visible_event_each() {
    use std::sync::{Arc, Barrier};

    let fixture = Fixture::new("concurrent relay reminders");
    let report = fixture.report("worker-a", "The completed result needs a decision.", None);
    fixture.handoff(&report, "turn-concurrent-reminder");
    let mut source_delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    source_delivery.ts = (Utc::now() - Duration::seconds(90)).to_rfc3339();
    source_delivery.by = "system".into();
    source_delivery.data = json!({
        "actor_id":"web-lead","source_event_id":report.id,
        "delivery_id":"delivery-concurrent-source","state":"accepted",
        "transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("source delivery");
    ledger::append(&fixture.path(), &source_delivery).expect("source delivery");

    let barrier = Arc::new(Barrier::new(10));
    let reminder_results = std::thread::scope(|scope| {
        (0..10)
            .map(|_| {
                let barrier = barrier.clone();
                let home = &fixture.home;
                let group_id = fixture.group.group_id.clone();
                scope.spawn(move || {
                    barrier.wait();
                    remind_due(
                        home,
                        &DaemonRequest {
                            v: 1,
                            op: "coordination_relay_remind".into(),
                            args: json!({
                                "group_id":group_id,"actor_id":"web-lead","by":"web-lead",
                                "browser_idle":false
                            })
                            .as_object()
                            .cloned()
                            .expect("request"),
                        },
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("reminder thread"))
            .collect::<Vec<_>>()
    });
    assert!(
        reminder_results.iter().all(Result::is_ok),
        "{reminder_results:?}"
    );
    let events = fixture.events();
    let reminders = events
        .iter()
        .filter(|event| {
            event.data.get("relay_kind").and_then(Value::as_str) == Some("decision_reminder")
        })
        .collect::<Vec<_>>();
    assert_eq!(reminders.len(), 1);
    let reminder_id = reminders[0].id.clone();
    let mut reminder_delivery = Event::new("runtime.delivery", &fixture.group.group_id);
    reminder_delivery.ts = (Utc::now() - Duration::seconds(40)).to_rfc3339();
    reminder_delivery.by = "system".into();
    reminder_delivery.data = json!({
        "actor_id":"web-lead","source_event_id":reminder_id,
        "delivery_id":"delivery-concurrent-reminder","state":"accepted",
        "transport":"web_model_browser"
    })
    .as_object()
    .cloned()
    .expect("reminder delivery");
    ledger::append(&fixture.path(), &reminder_delivery).expect("reminder delivery");

    let barrier = Arc::new(Barrier::new(10));
    let escalation_results = std::thread::scope(|scope| {
        (0..10)
            .map(|_| {
                let barrier = barrier.clone();
                let home = &fixture.home;
                let group_id = fixture.group.group_id.clone();
                scope.spawn(move || {
                    barrier.wait();
                    remind_due(
                        home,
                        &DaemonRequest {
                            v: 1,
                            op: "coordination_relay_remind".into(),
                            args: json!({
                                "group_id":group_id,"actor_id":"web-lead","by":"web-lead",
                                "browser_idle":true
                            })
                            .as_object()
                            .cloned()
                            .expect("request"),
                        },
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("escalation thread"))
            .collect::<Vec<_>>()
    });
    assert!(
        escalation_results.iter().all(Result::is_ok),
        "{escalation_results:?}"
    );
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.data.get("relay_kind").and_then(Value::as_str) == Some("decision_escalation")
            })
            .count(),
        1
    );
}

#[test]
fn concurrent_identical_decisions_commit_one_machine_decision_without_duplicate_output() {
    use std::sync::{Arc, Barrier};

    let fixture = Fixture::new("concurrent identical decision");
    let report = fixture.report("worker-a", "The result is ready for user approval.", None);
    fixture.handoff(&report, "turn-concurrent-decision");
    let barrier = Arc::new(Barrier::new(10));
    let results = std::thread::scope(|scope| {
        (0..10)
            .map(|_| {
                let barrier = barrier.clone();
                let home = &fixture.home;
                let group_id = fixture.group.group_id.clone();
                let event_id = report.id.clone();
                scope.spawn(move || {
                    barrier.wait();
                    decide(
                        home,
                        &DaemonRequest {
                            v: 1,
                            op: "coordination_decide".into(),
                            args: json!({
                                "group_id":group_id,"by":"web-lead","event_ids":[event_id],
                                "decision":"wait_user"
                            })
                            .as_object()
                            .cloned()
                            .expect("request"),
                        },
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("decision thread"))
            .collect::<Vec<_>>()
    });
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    let events = fixture.events();
    assert_eq!(relay_events(&events, DECISION_KIND).len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == "chat.message"
                    && event.by == "web-lead"
                    && event.data["to"] == json!(["user"])
            })
            .count(),
        0
    );
}

#[test]
fn replay_cannot_claim_success_for_a_batch_containing_a_new_report() {
    let fixture = Fixture::new("mixed handled and new sources");
    let first = fixture.report("worker-a", "Already reviewed.", None);
    fixture.handoff(&first, "first-turn");
    fixture
        .decide(json!({"event_ids":[first.id],"decision":"wait_user"}))
        .expect("first decision");
    let second = fixture.report("worker-b", "New result still needs review.", None);
    let handoff = fixture.handoff(&second, "second-turn");
    let error = fixture
        .decide(json!({
            "event_ids":[first.id, second.id],"decision":"wait_user"
        }))
        .expect_err("a partial overlap must not report the whole batch as applied");
    assert_eq!(error.code, "relay_decision_conflict");
    let events = fixture.events();
    assert_eq!(relay_events(&events, DECISION_KIND).len(), 1);
    assert_eq!(
        unresolved_source_ids(&events, &handoff),
        vec![second.id.clone()]
    );
    fixture
        .decide(json!({"event_ids":[second.id],"decision":"wait_user"}))
        .expect("the new source can still be decided separately");
    assert_eq!(relay_events(&fixture.events(), DECISION_KIND).len(), 2);
}

#[test]
fn recording_wait_during_a_paused_active_turn_does_not_report_a_false_failure() {
    let mut fixture = Fixture::new("paused active relay");
    let report = fixture.report("worker-a", "Please review before resuming.", None);
    fixture.handoff(&report, "paused-active-turn");
    fixture.claim_for_web(&report);
    let request = DaemonRequest {
        v: 1,
        op: "runtime_wait_next_turn".into(),
        args: json!({"group_id":fixture.group.group_id,"actor_id":"web-lead",
            "by":"web-lead","transport":"web_model_browser"})
        .as_object()
        .cloned()
        .expect("wait args"),
    };
    let wait = super::super::runtime_state::handle(&fixture.home, &request)
        .expect("wait operation")
        .expect("active turn");
    assert_eq!(wait["status"], "work_available");
    fixture.group.state = GroupState::Paused;
    GroupStore::new(fixture.home.clone())
        .expect("store")
        .save(&fixture.group)
        .expect("pause group");
    let result = fixture
        .decide(json!({"event_ids":[report.id],"decision":"wait_user"}))
        .expect("recording responsibility must not require restarting a paused actor");
    assert_eq!(result["current_responsibility"]["kind"], "user_pause");
    assert_eq!(result["caller_may_idle"], true);
    let replay = fixture
        .decide(json!({"event_ids":[report.id],"decision":"wait_user"}))
        .expect("retry remains successful while paused");
    assert_eq!(replay["replayed"], true);
    assert_eq!(relay_events(&fixture.events(), DECISION_KIND).len(), 1);
    assert_eq!(
        GroupStore::new(fixture.home.clone())
            .expect("store")
            .load(&fixture.group.group_id)
            .expect("group")
            .state,
        GroupState::Paused
    );
}
