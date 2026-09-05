use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{HomeLayout, fs, settings};

const LEGACY_SETTINGS_KEY: &str = "web_model_connectors";

fn store_path(home: &HomeLayout) -> PathBuf {
    home.root().join("web_model_connectors.yaml")
}

fn lock_path(home: &HomeLayout) -> PathBuf {
    store_path(home).with_extension("yaml.lock")
}

fn hash_secret(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

fn secret_preview(secret: &str) -> String {
    if secret.chars().count() <= 10 {
        return "****".into();
    }
    let prefix = secret.chars().take(6).collect::<String>();
    let suffix = secret
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}...{suffix}")
}

fn normalized_entry(connector_id: &str, raw: &Value) -> Option<Value> {
    let connector_id = connector_id.trim();
    let mut item = raw.as_object()?.clone();
    let group_id = item.get("group_id")?.as_str()?.trim().to_owned();
    let actor_id = item.get("actor_id")?.as_str()?.trim().to_owned();
    if connector_id.is_empty() || group_id.is_empty() || actor_id.is_empty() {
        return None;
    }
    let secret = item
        .get("secret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let secret_hash = item
        .get("secret_hash")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if secret.is_empty() && secret_hash.is_empty() {
        return None;
    }
    item.insert("connector_id".into(), json!(connector_id));
    item.insert("group_id".into(), json!(group_id));
    item.insert("actor_id".into(), json!(actor_id));
    item.entry("kind")
        .or_insert_with(|| json!("web_model_connector"));
    if secret_hash.is_empty() {
        item.insert("secret_hash".into(), json!(hash_secret(&secret)));
    }
    if item
        .get("secret_preview")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
        && !secret.is_empty()
    {
        item.insert("secret_preview".into(), json!(secret_preview(&secret)));
    }
    item.entry("revoked").or_insert_with(|| Value::Bool(false));
    Some(Value::Object(item))
}

fn connector_map(raw: &Value) -> Map<String, Value> {
    let mut result = Map::new();
    if let Some(items) = raw.as_array() {
        for item in items {
            let id = item["connector_id"].as_str().unwrap_or("");
            if let Some(item) = normalized_entry(id, item) {
                let id = item["connector_id"]
                    .as_str()
                    .expect("normalized connector id")
                    .to_owned();
                result.insert(id, item);
            }
        }
        return collapse_active_duplicates(result);
    }
    let Some(root) = raw.as_object() else {
        return result;
    };
    let items = root
        .get("connectors")
        .and_then(Value::as_object)
        .unwrap_or(root);
    for (id, item) in items {
        if let Some(item) = normalized_entry(id, item) {
            let id = item["connector_id"].as_str().unwrap_or(id).to_owned();
            result.insert(id, item);
        }
    }
    collapse_active_duplicates(result)
}

fn entry_rank(item: &Value, connector_id: &str) -> (String, String, String, String) {
    (
        item["created_at"].as_str().unwrap_or("").to_owned(),
        item["updated_at"].as_str().unwrap_or("").to_owned(),
        item["last_activity_at"].as_str().unwrap_or("").to_owned(),
        connector_id.to_owned(),
    )
}

fn collapse_active_duplicates(mut connectors: Map<String, Value>) -> Map<String, Value> {
    let mut current_by_actor = BTreeMap::<(String, String), String>::new();
    for (connector_id, item) in &connectors {
        if item["revoked"].as_bool().unwrap_or(false) {
            continue;
        }
        let group_id = item["group_id"].as_str().unwrap_or("").to_owned();
        let actor_id = item["actor_id"].as_str().unwrap_or("").to_owned();
        if group_id.is_empty() || actor_id.is_empty() {
            continue;
        }
        let key = (group_id, actor_id);
        let replace = current_by_actor
            .get(&key)
            .and_then(|current_id| {
                connectors
                    .get(current_id)
                    .map(|current| entry_rank(item, connector_id) > entry_rank(current, current_id))
            })
            .unwrap_or(true);
        if replace {
            current_by_actor.insert(key, connector_id.clone());
        }
    }
    let current_ids = current_by_actor
        .into_values()
        .collect::<std::collections::BTreeSet<_>>();
    for (connector_id, item) in &mut connectors {
        if item["revoked"].as_bool().unwrap_or(false) || current_ids.contains(connector_id) {
            continue;
        }
        item["revoked"] = Value::Bool(true);
        if item["updated_at"].as_str().unwrap_or("").is_empty() {
            item["updated_at"] = item["created_at"].clone();
        }
    }
    connectors
}

fn merge_maps(
    mut canonical: Map<String, Value>,
    imported: Map<String, Value>,
) -> Map<String, Value> {
    let retired_routes = canonical
        .values()
        .filter(|item| item["revoked"].as_bool().unwrap_or(false))
        .filter_map(|item| {
            let group_id = item["group_id"].as_str()?.trim();
            let actor_id = item["actor_id"].as_str()?.trim();
            (!group_id.is_empty() && !actor_id.is_empty())
                .then(|| (group_id.to_owned(), actor_id.to_owned()))
        })
        .collect::<std::collections::BTreeSet<_>>();
    for (connector_id, incoming) in imported {
        let route = (
            incoming["group_id"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_owned(),
            incoming["actor_id"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_owned(),
        );
        let Some(existing) = canonical.get(&connector_id) else {
            if !incoming["revoked"].as_bool().unwrap_or(false) && retired_routes.contains(&route) {
                continue;
            }
            canonical.insert(connector_id, incoming);
            continue;
        };
        let mut merged = incoming.as_object().cloned().unwrap_or_default();
        if let Some(existing) = existing.as_object() {
            merged.extend(existing.clone());
        }
        canonical.insert(connector_id, Value::Object(merged));
    }
    collapse_active_duplicates(canonical)
}

fn read_unlocked(path: &Path) -> io::Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    Ok(connector_map(&fs::read_yaml::<Value>(path)?))
}

fn write_unlocked(path: &Path, connectors: &Map<String, Value>) -> io::Result<()> {
    fs::write_secret_yaml(path, &json!({"connectors":connectors}))
}

fn migrate_settings_store(home: &HomeLayout) -> io::Result<()> {
    if !settings::load(home)?
        .extra
        .contains_key(LEGACY_SETTINGS_KEY)
    {
        return Ok(());
    }
    settings::update(home, |global| {
        let Some(legacy) = global.extra.get(LEGACY_SETTINGS_KEY).cloned() else {
            return Ok(());
        };
        let imported = connector_map(&legacy);
        fs::with_exclusive_lock(&lock_path(home), || {
            let path = store_path(home);
            let canonical = read_unlocked(&path)?;
            if !imported.is_empty() {
                write_unlocked(&path, &merge_maps(canonical, imported))?;
            }
            Ok(())
        })?;
        global.extra.remove(LEGACY_SETTINGS_KEY);
        Ok(())
    })
}

fn update<T>(
    home: &HomeLayout,
    change: impl FnOnce(&mut Map<String, Value>) -> io::Result<T>,
) -> io::Result<T> {
    migrate_settings_store(home)?;
    fs::with_exclusive_lock(&lock_path(home), || {
        let path = store_path(home);
        let mut connectors = read_unlocked(&path)?;
        let result = change(&mut connectors)?;
        write_unlocked(&path, &collapse_active_duplicates(connectors))?;
        Ok(result)
    })
}

pub fn load(home: &HomeLayout) -> io::Result<Vec<Value>> {
    migrate_settings_store(home)?;
    fs::with_exclusive_lock(&lock_path(home), || {
        Ok(read_unlocked(&store_path(home))?.into_values().collect())
    })
}

pub fn replace_active(home: &HomeLayout, connector: &Value) -> io::Result<Vec<String>> {
    update(home, |items| {
        let id = connector["connector_id"].as_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "connector_id is required")
        })?;
        let connector = normalized_entry(id, connector).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid web-model connector")
        })?;
        let mut replaced = Vec::new();
        let now = cccc_contracts::utc_now();
        for item in items.values_mut() {
            let same = item["group_id"] == connector["group_id"]
                && item["actor_id"] == connector["actor_id"]
                && !item["revoked"].as_bool().unwrap_or(false);
            if !same {
                continue;
            }
            if let Some(id) = item["connector_id"].as_str() {
                replaced.push(id.to_owned());
            }
            item["revoked"] = Value::Bool(true);
            item["updated_at"] = json!(now);
        }
        let id = connector["connector_id"]
            .as_str()
            .expect("normalized connector id")
            .to_owned();
        items.insert(id, connector);
        Ok(replaced)
    })
}

pub fn revoke(home: &HomeLayout, connector_id: &str) -> io::Result<bool> {
    update(home, |items| {
        let Some(item) = items.get_mut(connector_id) else {
            return Ok(false);
        };
        item["revoked"] = Value::Bool(true);
        item["updated_at"] = json!(cccc_contracts::utc_now());
        Ok(true)
    })
}

pub fn retire_actor(home: &HomeLayout, group_id: &str, actor_id: &str) -> io::Result<Vec<Value>> {
    update(home, |items| {
        let mut retired = Vec::new();
        let now = cccc_contracts::utc_now();
        for item in items.values_mut() {
            if item["group_id"] != group_id
                || item["actor_id"] != actor_id
                || item["revoked"].as_bool().unwrap_or(false)
            {
                continue;
            }
            retired.push(item.clone());
            item["revoked"] = Value::Bool(true);
            item["updated_at"] = json!(now);
        }
        Ok(retired)
    })
}

pub fn retire_group(home: &HomeLayout, group_id: &str) -> io::Result<Vec<Value>> {
    update(home, |items| {
        let mut retired = Vec::new();
        let now = cccc_contracts::utc_now();
        for item in items.values_mut() {
            if item["group_id"] != group_id || item["revoked"].as_bool().unwrap_or(false) {
                continue;
            }
            retired.push(item.clone());
            item["revoked"] = Value::Bool(true);
            item["updated_at"] = json!(now);
        }
        Ok(retired)
    })
}

pub fn restore(home: &HomeLayout, entries: &[Value]) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    update(home, |items| {
        for entry in entries {
            let id = entry["connector_id"].as_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "connector_id is required")
            })?;
            let normalized = normalized_entry(id, entry).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid web-model connector")
            })?;
            items.insert(id.to_owned(), normalized);
        }
        Ok(())
    })
}

pub fn update_connector(
    home: &HomeLayout,
    connector_id: &str,
    change: impl FnOnce(&mut Value),
) -> io::Result<bool> {
    update(home, |items| {
        let Some(item) = items.get_mut(connector_id) else {
            return Ok(false);
        };
        change(item);
        Ok(true)
    })
}

pub fn secret_matches(item: &Value, supplied: &str) -> bool {
    item["secret"].as_str() == Some(supplied)
        || item["secret_hash"].as_str() == Some(hash_secret(supplied).as_str())
}

// Session routing reuses the connector store and its cross-process lock. The
// caller must obtain the session from the trusted transport, never tool arguments.
pub fn find_session(home: &HomeLayout, session: &str) -> io::Result<Option<Value>> {
    let result = find_binding_hash(home, "session_hash", session)?;
    if let Some(item) = &result {
        validate_session_actor(home, item)?;
    }
    Ok(result)
}

/// Locate a route only; `bind_session` revalidates the actor and consumes the code.
pub fn find_binding_code(home: &HomeLayout, code: &str) -> io::Result<Option<Value>> {
    find_binding_hash(home, "binding_code_hash", code)
}

fn find_binding_hash(home: &HomeLayout, field: &str, raw: &str) -> io::Result<Option<Value>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let wanted = hash_secret(raw);
    let mut matches = load(home)?.into_iter().filter(|item| {
        item["kind"] == "web_model_connector"
            && item["revoked"] != true
            && item[field].as_str() == Some(wanted.as_str())
    });
    let found = matches.next();
    if matches.next().is_some() {
        return Err(io::Error::other("connector_binding_conflict"));
    }
    Ok(found)
}

/// Issue a replacement code without changing the currently bound conversation.
/// Only the returned value contains the plaintext code; storage keeps its hash.
pub fn prepare_binding(
    home: &HomeLayout,
    connector_id: &str,
    ttl_seconds: i64,
) -> io::Result<Value> {
    let code = format!(
        "cccb_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(ttl_seconds.clamp(0, 3600));
    update(home, |items| {
        let item = items
            .get_mut(connector_id.trim())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connector_not_found"))?;
        validate_session_actor(home, item)?;
        item["binding_code_hash"] = json!(hash_secret(&code));
        item["binding_expires_at"] = json!(expires.to_rfc3339());
        item["updated_at"] = json!(now.to_rfc3339());
        Ok(json!({"code":code,"binding_expires_at":expires.to_rfc3339()}))
    })
}

/// Consume a code and replace its session under the existing store lock.
/// This binds the inbound route only; it does not claim browser delivery works.
pub fn bind_session(
    home: &HomeLayout,
    connector_id: &str,
    code: &str,
    session: &str,
) -> io::Result<Value> {
    let session = session.trim();
    let code = code.trim();
    if session.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session_binding_required",
        ));
    }
    if code.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session_binding_code_invalid",
        ));
    }
    let connector_id = connector_id.trim();
    let session_hash = hash_secret(session);
    let code_hash = hash_secret(code);
    update(home, |items| {
        let item = items
            .get(connector_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connector_not_found"))?;
        validate_session_actor(home, item)?;
        if item["binding_code_hash"].as_str() != Some(code_hash.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session_binding_code_invalid",
            ));
        }
        let now = chrono::Utc::now();
        let expires = item["binding_expires_at"]
            .as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        if expires.is_none_or(|value| value <= now) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "session_binding_code_expired",
            ));
        }
        if items.iter().any(|(id, other)| {
            id != connector_id
                && other["kind"] == "web_model_connector"
                && other["revoked"] != true
                && other["session_hash"].as_str() == Some(session_hash.as_str())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session_already_bound",
            ));
        }
        let item = items
            .get_mut(connector_id)
            .expect("connector validated under lock");
        item["session_hash"] = json!(session_hash);
        item["session_bound_at"] = json!(now.to_rfc3339());
        item["binding_code_hash"] = json!("");
        item["binding_expires_at"] = json!("");
        item["updated_at"] = json!(now.to_rfc3339());
        Ok(json!({
            "bound":true,
            "connector_id":connector_id,
            "group_id":item["group_id"],
            "actor_id":item["actor_id"]
        }))
    })
}

fn validate_session_actor(home: &HomeLayout, connector: &Value) -> io::Result<()> {
    if connector["kind"] != "web_model_connector" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid_connector_kind",
        ));
    }
    if connector["revoked"] == true {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "connector_revoked",
        ));
    }
    let group_id = connector["group_id"].as_str().unwrap_or_default();
    let actor_id = connector["actor_id"].as_str().unwrap_or_default();
    let group = crate::GroupStore::new(home.clone())?.load(group_id)?;
    let valid = crate::actors::find(&group, actor_id).is_some_and(|actor| {
        actor.enabled
            && actor.runtime == cccc_contracts::ActorRuntime::WebModel
            && crate::actors::effective_role(&group, actor_id)
                == Some(cccc_contracts::ActorRole::Foreman)
    });
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "connector_actor_unavailable",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_revocation_wins_over_a_newer_legacy_settings_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let connector = |revoked: bool, updated_at: &str| {
            json!({
                "connector_id":"wmc_retired",
                "group_id":"g_test",
                "actor_id":"web1",
                "provider":"chatgpt",
                "secret_hash":hash_secret("fixture-secret"),
                "revoked":revoked,
                "created_at":"2026-08-28T00:00:00Z",
                "updated_at":updated_at,
            })
        };
        write_unlocked(
            &store_path(&home),
            &Map::from_iter([(
                "wmc_retired".into(),
                connector(true, "2026-08-28T00:00:01Z"),
            )]),
        )
        .expect("canonical connector");
        settings::update(&home, |global| {
            global.extra.insert(
                LEGACY_SETTINGS_KEY.into(),
                json!({
                    "wmc_retired":connector(false, "2026-08-28T00:00:02Z"),
                    "wmc_retired_alias":{
                        "connector_id":" wmc_retired_alias ",
                        "group_id":" g_test ",
                        "actor_id":"web1 ",
                        "provider":"chatgpt",
                        "secret_hash":hash_secret("legacy-alias-secret"),
                        "revoked":false,
                        "created_at":"2026-08-28T00:00:02Z",
                        "updated_at":"2026-08-28T00:00:02Z",
                    }
                }),
            );
            Ok(())
        })
        .expect("legacy settings connector");

        let connectors = load(&home).expect("migrated connectors");
        let connector = connectors
            .iter()
            .find(|item| item["connector_id"] == "wmc_retired")
            .expect("retired connector");
        assert_eq!(connector["revoked"], true);
        assert_eq!(connectors.len(), 1);
        assert!(
            !settings::load(&home)
                .expect("settings")
                .extra
                .contains_key(LEGACY_SETTINGS_KEY)
        );
    }

    // Store-level fixtures intentionally bypass the daemon's separate singleton
    // policy. These tests do not claim multi-group browser support is enabled.
    fn session_fixture() -> (tempfile::TempDir, HomeLayout, Vec<String>) {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = crate::GroupStore::new(home.clone()).expect("store");
        let mut groups = Vec::new();
        for id in ["route-a", "route-b"] {
            let mut group = store.create(id, "").expect("group");
            let mut actor = cccc_contracts::Actor::new("web-lead");
            actor.runtime = cccc_contracts::ActorRuntime::WebModel;
            crate::actors::add(&mut group, actor).expect("actor");
            store.save(&group).expect("save group");
            replace_active(
                &home,
                &json!({
                    "connector_id":id,"group_id":group.group_id,"actor_id":"web-lead",
                    "secret_hash":hash_secret("fixture-only"),"provider":"chatgpt"
                }),
            )
            .expect("connector");
            groups.push(group.group_id);
        }
        (temp, home, groups)
    }

    fn issue(home: &HomeLayout, connector: &str) -> String {
        prepare_binding(home, connector, 600).expect("prepare")["code"]
            .as_str()
            .expect("code")
            .to_owned()
    }

    #[test]
    fn session_binding_preserves_old_owner_until_single_use_replacement_commits() {
        let (_temp, home, groups) = session_fixture();
        let first = issue(&home, "route-a");
        assert_eq!(
            find_binding_code(&home, &first)
                .expect("fixture operation succeeds")
                .expect("fixture operation succeeds")["connector_id"],
            "route-a"
        );
        let bound = bind_session(&home, "route-a", &first, "chat-original")
            .expect("fixture operation succeeds");
        assert_eq!(bound["group_id"], groups[0]);
        assert!(bound.get("secret").is_none());
        let next = issue(&home, "route-a");
        assert!(
            find_session(&home, "chat-original")
                .expect("fixture operation succeeds")
                .is_some()
        );
        assert_eq!(
            bind_session(&home, "route-a", "wrong", "chat-replacement")
                .expect_err("binding must be rejected")
                .to_string(),
            "session_binding_code_invalid"
        );
        assert!(
            find_session(&home, "chat-original")
                .expect("fixture operation succeeds")
                .is_some()
        );
        bind_session(&home, "route-a", &next, "chat-replacement")
            .expect("fixture operation succeeds");
        assert!(
            find_session(&home, "chat-original")
                .expect("fixture operation succeeds")
                .is_none()
        );
        assert!(
            find_binding_code(&home, &next)
                .expect("fixture operation succeeds")
                .is_none()
        );
        assert_eq!(
            bind_session(&home, "route-a", &next, "chat-attacker")
                .expect_err("binding must be rejected")
                .to_string(),
            "session_binding_code_invalid"
        );
        let restarted =
            HomeLayout::from_path(home.root().to_path_buf()).expect("fixture operation succeeds");
        assert_eq!(
            find_session(&restarted, "chat-replacement")
                .expect("fixture operation succeeds")
                .expect("fixture operation succeeds")["group_id"],
            groups[0]
        );
        let disk = std::fs::read_to_string(store_path(&home)).expect("fixture operation succeeds");
        for secret in [&first, &next, "chat-original", "chat-replacement"] {
            assert!(!disk.contains(secret), "raw binding material was persisted");
        }
    }

    #[test]
    fn session_binding_rejects_cross_group_reuse_without_consuming_the_other_code() {
        let (_temp, home, groups) = session_fixture();
        let a = issue(&home, "route-a");
        let b = issue(&home, "route-b");
        bind_session(&home, "route-a", &a, "chat-a").expect("fixture operation succeeds");
        assert_eq!(
            bind_session(&home, "route-b", &b, "chat-a")
                .expect_err("binding must be rejected")
                .to_string(),
            "session_already_bound"
        );
        bind_session(&home, "route-b", &b, "chat-b").expect("fixture operation succeeds");
        assert_eq!(
            find_session(&home, "chat-a")
                .expect("fixture operation succeeds")
                .expect("fixture operation succeeds")["group_id"],
            groups[0]
        );
        assert_eq!(
            find_session(&home, "chat-b")
                .expect("fixture operation succeeds")
                .expect("fixture operation succeeds")["group_id"],
            groups[1]
        );
        assert!(
            find_session(&home, "")
                .expect("fixture operation succeeds")
                .is_none()
        );
        assert!(
            find_binding_code(&home, "")
                .expect("fixture operation succeeds")
                .is_none()
        );
    }

    #[test]
    fn session_binding_expiry_and_revocation_keep_the_current_owner() {
        let (_temp, home, _) = session_fixture();
        bind_session(&home, "route-a", &issue(&home, "route-a"), "active-chat")
            .expect("fixture operation succeeds");
        let expired =
            prepare_binding(&home, "route-a", 0).expect("fixture operation succeeds")["code"]
                .as_str()
                .expect("fixture operation succeeds")
                .to_owned();
        assert_eq!(
            bind_session(&home, "route-a", &expired, "new-chat")
                .expect_err("binding must be rejected")
                .to_string(),
            "session_binding_code_expired"
        );
        assert!(
            find_session(&home, "active-chat")
                .expect("fixture operation succeeds")
                .is_some()
        );
        let cancelled = issue(&home, "route-a");
        let current = issue(&home, "route-a");
        assert!(bind_session(&home, "route-a", &cancelled, "new-chat").is_err());
        assert_eq!(
            bind_session(&home, "route-a", "", "new-chat")
                .expect_err("binding must be rejected")
                .to_string(),
            "session_binding_code_invalid"
        );
        assert_eq!(
            bind_session(&home, "route-a", &current, " ")
                .expect_err("binding must be rejected")
                .to_string(),
            "session_binding_required"
        );
        revoke(&home, "route-a").expect("fixture operation succeeds");
        assert!(
            find_session(&home, "active-chat")
                .expect("fixture operation succeeds")
                .is_none()
        );
        assert_eq!(
            bind_session(&home, "route-a", &current, "new-chat")
                .expect_err("binding must be rejected")
                .to_string(),
            "connector_revoked"
        );
    }

    #[test]
    fn session_binding_revalidates_disabled_replaced_and_demoted_foremen() {
        let (_temp, home, groups) = session_fixture();
        let store = crate::GroupStore::new(home.clone()).expect("fixture operation succeeds");
        let original = store.load(&groups[0]).expect("fixture operation succeeds");
        bind_session(&home, "route-a", &issue(&home, "route-a"), "active-chat")
            .expect("fixture operation succeeds");
        let pending = issue(&home, "route-a");
        for change in ["disabled", "local", "demoted", "removed"] {
            let mut group = original.clone();
            match change {
                "disabled" => group.actors[0].enabled = false,
                "local" => group.actors[0].runtime = cccc_contracts::ActorRuntime::Codex,
                "demoted" => group
                    .actors
                    .insert(0, cccc_contracts::Actor::new("new-lead")),
                "removed" => group.actors.clear(),
                _ => unreachable!(),
            }
            store.save(&group).expect("fixture operation succeeds");
            assert_eq!(
                bind_session(&home, "route-a", &pending, "new-chat")
                    .expect_err("binding must be rejected")
                    .to_string(),
                "connector_actor_unavailable"
            );
            assert!(find_session(&home, "active-chat").is_err());
            assert!(prepare_binding(&home, "route-a", 600).is_err());
        }
        store.save(&original).expect("fixture operation succeeds");
        bind_session(&home, "route-a", &pending, "new-chat").expect("fixture operation succeeds");
        assert!(
            find_session(&home, "new-chat")
                .expect("fixture operation succeeds")
                .is_some()
        );
    }

    #[test]
    fn session_binding_parallel_claims_have_exactly_one_winner() {
        for same_code in [true, false] {
            let (_temp, home, _) = session_fixture();
            let a = issue(&home, "route-a");
            let b = if same_code {
                a.clone()
            } else {
                issue(&home, "route-b")
            };
            let barrier = std::sync::Barrier::new(2);
            let results = std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    barrier.wait();
                    bind_session(&home, "route-a", &a, "chat-one")
                });
                let second = scope.spawn(|| {
                    barrier.wait();
                    bind_session(
                        &home,
                        if same_code { "route-a" } else { "route-b" },
                        &b,
                        if same_code { "chat-two" } else { "chat-one" },
                    )
                });
                [
                    first.join().expect("fixture operation succeeds"),
                    second.join().expect("fixture operation succeeds"),
                ]
            });
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
            let error = results
                .into_iter()
                .find_map(Result::err)
                .expect("fixture operation succeeds");
            assert_eq!(
                error.to_string(),
                if same_code {
                    "session_binding_code_invalid"
                } else {
                    "session_already_bound"
                }
            );
        }
    }

    #[test]
    fn session_binding_respects_the_existing_lock_across_processes() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        const CHILD_HOME: &str = "CCCC_TEST_SESSION_BINDING_CHILD_HOME";
        if let Some(path) = std::env::var_os(CHILD_HOME) {
            let home = HomeLayout::from_path(PathBuf::from(path)).expect("child home");
            let code = std::env::var("CCCC_TEST_SESSION_BINDING_CODE").expect("fixture code");
            std::fs::write(home.root().join("child-started"), "ready").expect("signal started");
            bind_session(&home, "route-a", &code, "child-chat").expect("child binding");
            std::fs::write(home.root().join("child-bound"), "bound").expect("signal bound");
            return;
        }
        let (_temp, home, _) = session_fixture();
        let code = issue(&home, "route-a");
        let (mut child, blocked) = fs::with_exclusive_lock(&lock_path(&home), || {
            let mut child = Command::new(std::env::current_exe()?)
                .args(["--exact", "web_model_connectors::tests::session_binding_respects_the_existing_lock_across_processes"])
                .env(CHILD_HOME, home.root())
                .env("CCCC_TEST_SESSION_BINDING_CODE", &code)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !home.root().join("child-started").exists()
                && child.try_wait()?.is_none() && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(200));
            let blocked = home.root().join("child-started").exists()
                && !home.root().join("child-bound").exists()
                && child.try_wait()?.is_none();
            Ok((child, blocked))
        }).expect("hold existing store lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("child status") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not resume after the store lock was released");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            blocked,
            "another process bypassed the existing connector lock"
        );
        assert!(
            status.success(),
            "child could not bind after releasing the lock"
        );
        assert!(
            find_session(&home, "child-chat")
                .expect("read child binding")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_binding_write_failure_does_not_consume_code_or_replace_owner() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, home, _) = session_fixture();
        bind_session(&home, "route-a", &issue(&home, "route-a"), "old-chat")
            .expect("fixture operation succeeds");
        let pending = issue(&home, "route-a");
        std::fs::set_permissions(home.root(), std::fs::Permissions::from_mode(0o500))
            .expect("fixture operation succeeds");
        let failed = bind_session(&home, "route-a", &pending, "new-chat");
        std::fs::set_permissions(home.root(), std::fs::Permissions::from_mode(0o700))
            .expect("fixture operation succeeds");
        assert_eq!(
            failed.expect_err("binding must be rejected").kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            find_session(&home, "old-chat")
                .expect("fixture operation succeeds")
                .is_some()
        );
        assert!(
            find_session(&home, "new-chat")
                .expect("fixture operation succeeds")
                .is_none()
        );
        bind_session(&home, "route-a", &pending, "new-chat").expect("fixture operation succeeds");
    }
}
