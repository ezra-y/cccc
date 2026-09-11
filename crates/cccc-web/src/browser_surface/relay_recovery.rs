//! Confirm the latest conversation before recovering an outdated busy page.
use super::prompt_submission::{SELECT_COMPOSER_SCRIPT, inspect_submission, sign_in_required};
use super::{BrowserSurfaces, navigation::goto_dom_content_loaded};
use anyhow::{Context, Result};
use chromiumoxide::{Page, cdp::browser_protocol::target::CreateTargetParams};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RECHECK_INTERVAL: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct RelayProbe {
    page: Page,
    source: Page,
    owner: String,
    url: String,
    source_turn: String,
    pub(super) started: Instant,
    completed_turn: Option<String>,
}

impl BrowserSurfaces {
    pub(crate) async fn cancel_relay_probe(&self, key: &str, owner: &str) {
        let probe = self.sessions.lock().await.get_mut(key).and_then(|s| {
            if s.relay_probe.as_ref().is_some_and(|p| p.owner == owner) {
                s.relay_probe.take()
            } else {
                None
            }
        });
        if let Some(probe) = probe {
            if let Err(error) = probe.page.close().await {
                tracing::warn!(%error, "Could not close the temporary conversation check");
            }
        }
    }

    /// Driven by the existing delivery poll under the shared browser lock.
    pub(crate) async fn reconcile_relay_page(&self, key: &str, owner: &str) -> Result<()> {
        let original = self.page(key).await?;
        let observed = inspect_submission(&original, "", &[]).await?;
        // Never refresh an explicit archive, login, verification or refusal page.
        // Recovery uses observations, not retries against an account-level block.
        if !observed.page_blocker.is_empty() || sign_in_required(&original).await? {
            self.cancel_relay_probe(key, owner).await;
            return Ok(());
        }
        if observed.composer_chars > 0
            || (!(observed.running || observed.stop_visible) && composer_present(&original).await?)
        {
            self.cancel_relay_probe(key, owner).await;
            if let Some(s) = self.sessions.lock().await.get_mut(key) {
                s.relay_probe_after = None;
                s.relay_probe_retry_delay = RECHECK_INTERVAL;
                if s.metadata["relay_recovery"]["state"] == "blocked" {
                    s.metadata["relay_recovery"] = json!({"state":"ready"});
                }
            }
            return Ok(());
        }
        let now = Instant::now();
        let probe = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(key)
                .context("browser surface is not active")?;
            let last = &session.metadata["relay_recovery"];
            if last["state"] == "blocked"
                && last["url"] == observed.url
                && last["source_turn"] == observed.latest_turn_id
            {
                return Ok(());
            }
            if let Some(probe) = &session.relay_probe {
                if probe.owner != owner {
                    return Ok(());
                }
                probe.clone()
            } else {
                if session.relay_probe_after.is_some_and(|after| now < after) {
                    return Ok(());
                }
                // ponytail: one temporary tab in the existing browser/profile;
                // no second browser, copied login, provider API, or polling service.
                session.relay_probe_after = Some(now + session.relay_probe_retry_delay);
                session.relay_probe_retry_delay =
                    (session.relay_probe_retry_delay * 2).min(Duration::from_secs(15 * 60));
                let mut target = CreateTargetParams::new(&observed.url);
                target.background = Some(true);
                let page = session.browser.new_page(target).await?;
                session.relay_probe = Some(RelayProbe {
                    page,
                    source: original,
                    owner: owner.to_owned(),
                    url: observed.url,
                    source_turn: observed.latest_turn_id,
                    started: now,
                    completed_turn: None,
                });
                return Ok(());
            }
        };
        if probe.source.target_id() != original.target_id()
            || probe.url != observed.url
            || probe.source_turn != observed.latest_turn_id
        {
            self.cancel_relay_probe(key, owner).await;
            return Ok(());
        }
        let fresh = inspect_submission(&probe.page, "", &[]).await?;
        let blocker = if sign_in_required(&probe.page).await? {
            "login_required"
        } else {
            fresh.page_blocker.as_str()
        };
        if !blocker.is_empty() {
            if let Some(session) = self.sessions.lock().await.get_mut(key) {
                session.metadata["relay_recovery"] = json!({
                    "state":"blocked", "reason":blocker, "url":probe.url,
                    "source_turn":probe.source_turn, "at":cccc_contracts::utc_now()
                });
            }
            self.cancel_relay_probe(key, owner).await;
            return Ok(());
        }
        let completed = completed_turn(&probe.page, &probe.url).await;
        let stable = completed
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|id| probe.completed_turn.as_ref() == Some(id));
        if stable {
            // Native user input may arrive while the temporary page loads.
            let latest = inspect_submission(&original, "", &[]).await?;
            let result = if latest.url == probe.url
                && latest.latest_turn_id == probe.source_turn
                && latest.composer_chars == 0
                && (latest.running || latest.stop_visible || !composer_present(&original).await?)
            {
                goto_dom_content_loaded(&original, &probe.url).await
            } else {
                self.cancel_relay_probe(key, owner).await;
                return Ok(());
            };
            self.cancel_relay_probe(key, owner).await;
            result?;
            if let Some(session) = self.sessions.lock().await.get_mut(key) {
                session.relay_probe_after = None;
                session.relay_probe_retry_delay = RECHECK_INTERVAL;
                session.metadata["relay_recovery"] = json!({
                    "state":"refreshed", "reason":"fresh_completed_turn", "url":probe.url,
                    "completed_turn":completed.ok().flatten(), "at":cccc_contracts::utc_now()
                });
            }
            tracing::info!(
                url = probe.url,
                "Recovered outdated busy page after confirming a completed answer"
            );
        } else if now.duration_since(probe.started) >= PROBE_TIMEOUT {
            self.cancel_relay_probe(key, owner).await;
        } else if let Some(p) = self
            .sessions
            .lock()
            .await
            .get_mut(key)
            .and_then(|s| s.relay_probe.as_mut())
        {
            p.completed_turn = completed.ok().flatten();
        }
        Ok(())
    }
}

pub(super) async fn completed_turn(page: &Page, expected_url: &str) -> Result<Option<String>> {
    let snapshot = inspect_submission(page, "", &[]).await?;
    if snapshot.url != expected_url
        || snapshot.running
        || snapshot.stop_visible
        || !snapshot.page_blocker.is_empty()
        || snapshot.composer_chars > 0
        || sign_in_required(page).await?
    {
        return Ok(None);
    }
    if !composer_present(page).await? {
        return Ok(None);
    }
    // An empty/loading page or an earlier completed answer is not a finished turn.
    let result: Value = page.evaluate(r#"(() => {
        const turns = [...document.querySelectorAll('[data-testid^="conversation-turn"], main article')];
        const last = turns.at(-1);
        const answer = last?.querySelector('[data-message-author-role="assistant"]');
        if (!answer || !last.querySelector('[data-testid="copy-turn-action-button"]')) return {turn_id:null};
        return {turn_id:answer.getAttribute('data-message-id') || last.getAttribute('data-turn-id') || null};
    })()"#).await?.into_value().context("read fresh completed conversation turn")?;
    Ok(result["turn_id"].as_str().map(str::to_owned))
}

pub(super) async fn composer_present(page: &Page) -> Result<bool> {
    let input: Value = page
        .evaluate(format!("({SELECT_COMPOSER_SCRIPT})()"))
        .await?
        .into_value()?;
    Ok(input["selector"].as_str().is_some_and(|s| !s.is_empty()))
}
