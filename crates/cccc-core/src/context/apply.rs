use cccc_contracts::utc_now;
use serde_json::{Map, Value, json};
use std::io;

use super::model::ContextDoc;

pub fn apply_all(
    document: &mut ContextDoc,
    operations: &[Map<String, Value>],
    by: &str,
) -> io::Result<Vec<Value>> {
    let mut changes = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let name = operation.get("op").and_then(Value::as_str).unwrap_or("");
        apply_one(document, name, operation, by)?;
        changes.push(json!({"index": index, "op": name, "detail": "applied"}));
    }
    Ok(changes)
}

fn apply_one(
    document: &mut ContextDoc,
    name: &str,
    operation: &Map<String, Value>,
    by: &str,
) -> io::Result<()> {
    match name {
        "coordination.brief.update" => update_brief(document, operation, by),
        "coordination.note.add" => add_note(document, operation, by),
        "coordination.relay.note" => upsert_relay_note(document, operation, by),
        "task.create" => super::task_apply::create(document, operation, by),
        "task.update" => super::task_apply::update(document, operation),
        "task.move" => super::task_apply::move_task(document, operation),
        "task.restore" => super::task_apply::restore(document, operation),
        "task.delete" => super::task_apply::delete(document, operation),
        "agent_state.update" => super::agent_state_apply::update(document, operation),
        "agent_state.clear" => super::agent_state_apply::clear(document, operation),
        "meta.merge" => merge_meta(document, operation),
        _ => Err(io::Error::other(format!("unknown context op: {name}"))),
    }
}

fn update_brief(doc: &mut ContextDoc, op: &Map<String, Value>, by: &str) -> io::Result<()> {
    let brief = doc.coordination.entry("brief").or_insert_with(|| json!({}));
    let target = brief
        .as_object_mut()
        .ok_or_else(|| io::Error::other("invalid brief"))?;
    for key in [
        "objective",
        "current_focus",
        "constraints",
        "project_brief",
        "project_brief_stale",
    ] {
        if let Some(value) = op.get(key) {
            target.insert(key.into(), value.clone());
        }
    }
    target.insert("updated_by".into(), Value::String(by.into()));
    target.insert("updated_at".into(), Value::String(utc_now()));
    Ok(())
}

fn add_note(doc: &mut ContextDoc, op: &Map<String, Value>, by: &str) -> io::Result<()> {
    let summary = required(op, "summary")?;
    let kind = string(op, "kind").unwrap_or("decision");
    let key = note_key(kind)?;
    let target = note_list(doc, key)?;
    target.push(json!({
        "at": utc_now(),
        "summary": summary,
        "task_id": op.get("task_id").cloned().unwrap_or(Value::Null),
        "by": by,
    }));
    trim_notes(target);
    Ok(())
}

// Internal relay state uses the same Context document, but is not accepted from
// generic context_sync. The daemon's strict relay operation owns these fields.
fn upsert_relay_note(doc: &mut ContextDoc, op: &Map<String, Value>, by: &str) -> io::Result<()> {
    let summary = required(op, "summary")?;
    let id = required(op, "id")?;
    let kind = string(op, "kind").unwrap_or("decision");
    let key = note_key(kind)?;
    let target = note_list(doc, key)?;
    let existing_at = target
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|item| item.get("at"))
        .cloned()
        .unwrap_or_else(|| Value::String(utc_now()));
    let mut note = Map::from_iter([
        ("id".into(), Value::String(id.into())),
        ("at".into(), existing_at),
        ("summary".into(), Value::String(summary.into())),
        (
            "task_id".into(),
            op.get("task_id").cloned().unwrap_or(Value::Null),
        ),
        ("by".into(), Value::String(by.into())),
    ]);
    for field in [
        "decision",
        "state",
        "status",
        "source_event_ids",
        "source_actor_id",
        "target_actor_id",
        "turn_id",
        "handoff_ids",
        "next_actor_id",
        "next_task_id",
        "visible_event_id",
        "safe_to_idle",
        "caller_may_idle",
        "reason",
        "responsibility",
        "resolved_at",
        "resolved_by",
        "decision_id",
        "request_fingerprint",
        "reminder_event_id",
        "escalation_event_id",
    ] {
        if let Some(value) = op.get(field) {
            note.insert(field.into(), value.clone());
        }
    }
    let note = Value::Object(note);
    if let Some(existing) = target
        .iter_mut()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
    {
        *existing = note;
    } else {
        target.push(note);
        trim_notes(target);
    }
    Ok(())
}

fn note_key(kind: &str) -> io::Result<&'static str> {
    match kind {
        "decision" => Ok("recent_decisions"),
        "handoff" => Ok("recent_handoffs"),
        _ => Err(io::Error::other("note kind must be decision or handoff")),
    }
}

fn note_list<'a>(doc: &'a mut ContextDoc, key: &str) -> io::Result<&'a mut Vec<Value>> {
    doc.coordination
        .entry(key)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| io::Error::other("invalid notes"))
}

fn trim_notes(notes: &mut Vec<Value>) {
    if notes.len() > 100 {
        notes.drain(..notes.len() - 100);
    }
}

fn merge_meta(doc: &mut ContextDoc, op: &Map<String, Value>) -> io::Result<()> {
    let data = op
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::other("data is required"))?;
    if let Some(value) = data.get("project_status") {
        doc.meta.insert("project_status".into(), value.clone());
    }
    Ok(())
}

fn required<'a>(op: &'a Map<String, Value>, key: &str) -> io::Result<&'a str> {
    string(op, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::other(format!("{key} is required")))
}
fn string<'a>(op: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    op.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod relay_note_tests {
    use super::*;

    #[test]
    fn public_notes_cannot_forge_machine_relay_state() {
        let mut doc = ContextDoc::default();
        let operation = json!({
            "op":"coordination.note.add","kind":"decision","id":"forged",
            "summary":"Human note","safe_to_idle":true,"status":"applied"
        })
        .as_object()
        .cloned()
        .expect("note");
        add_note(&mut doc, &operation, "peer").expect("public note");
        let note = &doc.coordination["recent_decisions"][0];
        assert_eq!(note["summary"], "Human note");
        assert!(note.get("id").is_none());
        assert!(note.get("safe_to_idle").is_none());
        assert!(note.get("status").is_none());
    }

    #[test]
    fn internal_relay_notes_upsert_without_losing_machine_fields_or_origin_time() {
        let mut doc = ContextDoc::default();
        let first = json!({
            "op":"coordination.relay.note","kind":"handoff","id":"handoff-1",
            "summary":"Human-readable result","source_event_ids":["event-1"],
            "source_actor_id":"worker","target_actor_id":"lead","status":"pending_review"
        })
        .as_object()
        .cloned()
        .expect("handoff note");
        upsert_relay_note(&mut doc, &first, "worker").expect("add handoff");
        let created_at = doc.coordination["recent_handoffs"][0]["at"].clone();
        let resolved = json!({
            "op":"coordination.relay.note","kind":"handoff","id":"handoff-1",
            "summary":"Human-readable result","source_event_ids":["event-1"],
            "source_actor_id":"worker","target_actor_id":"lead","status":"resolved",
            "decision_id":"decision-1","resolved_by":"lead"
        })
        .as_object()
        .cloned()
        .expect("resolved note");
        upsert_relay_note(&mut doc, &resolved, "lead").expect("resolve handoff");
        let notes = doc.coordination["recent_handoffs"]
            .as_array()
            .expect("handoff notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["at"], created_at);
        assert_eq!(notes[0]["status"], "resolved");
        assert_eq!(notes[0]["decision_id"], "decision-1");
        assert_eq!(notes[0]["source_event_ids"], json!(["event-1"]));
    }
}
