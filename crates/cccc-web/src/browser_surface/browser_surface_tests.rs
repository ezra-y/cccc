use super::*;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod chrome_test_guard;
use chrome_test_guard::chrome_test_guard;

macro_rules! require_chrome {
    () => {
        if !chrome_available() {
            return;
        }
        let _chrome_guard = chrome_test_guard().await;
    };
}

#[test]
fn extracts_google_account_route_from_completion_url() {
    assert_eq!(
        authuser_from_url("https://notebooklm.google.com/?authuser=2"),
        2
    );
    assert_eq!(
        authuser_from_url("https://notebooklm.google.com/u/3/notebook/x"),
        3
    );
    assert_eq!(authuser_from_url("https://notebooklm.google.com/"), 0);
}

#[tokio::test]
async fn launches_chromium_and_captures_nonempty_frame() {
    require_chrome!();
    let (url, server) = local_page(
        "<!doctype html><html><body style='background:#fff'><h1>CCCC browser frame</h1><input autofocus></body></html>",
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let state = manager
        .open(
            "g_test::slot-1",
            &temp.path().join("profile"),
            &url,
            1120,
            760,
        )
        .await
        .expect("open");
    assert_eq!(state["state"], "ready");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let frame = manager.frame("g_test::slot-1").await.expect("frame");
    let image = base64::engine::general_purpose::STANDARD
        .decode(frame["data_base64"].as_str().expect("base64"))
        .expect("jpeg");
    assert!(image.len() > 1_000);
    assert_eq!(&image[..2], &[0xff, 0xd8]);
    assert_eq!(frame["width"], 1120);
    assert_eq!(frame["height"], 760);
    assert!(manager.close("g_test::slot-1").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn open_completes_after_dom_content_loaded_without_waiting_for_subresources() {
    require_chrome!();
    let (url, server) = page_with_stalled_subresource().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();

    let state = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        manager.open(
            "dom-content-loaded",
            &temp.path().join("profile"),
            &url,
            800,
            600,
        ),
    )
    .await
    .expect("open must not wait for the stalled image")
    .expect("open browser");

    assert_eq!(state["state"], "ready");
    let page = manager
        .sessions
        .lock()
        .await
        .get("dom-content-loaded")
        .expect("browser session")
        .page
        .clone();
    let heading: String = page
        .evaluate("document.querySelector('h1')?.textContent || ''")
        .await
        .expect("evaluate destination document")
        .into_value()
        .expect("heading text");
    assert_eq!(heading, "DOMContentLoaded");
    assert!(manager.close("dom-content-loaded").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn concurrent_ensure_open_reuses_one_profile_owner() {
    require_chrome!();
    let (url, server) = local_page("CCCC concurrent browser").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let profile = temp.path().join("profile");

    let (first, second) = tokio::join!(
        manager.ensure_open("web-model::g_test::actor", &profile, &url, 800, 600),
        manager.ensure_open("web-model::g_test::actor", &profile, &url, 800, 600),
    );
    let first = first.expect("first open");
    let second = second.expect("second open");

    assert_eq!(first["started_at"], second["started_at"]);
    assert_eq!(manager.sessions.lock().await.len(), 1);
    assert!(
        manager
            .close("web-model::g_test::actor")
            .await
            .expect("close")
    );
    server.abort();
}

#[tokio::test]
async fn reopens_same_profile_after_process_exit() {
    require_chrome!();
    let (url, server) = local_page("CCCC reopen browser").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let profile = temp.path().join("profile");

    manager
        .open("restartable", &profile, &url, 800, 600)
        .await
        .expect("first open");
    manager
        .open("restartable", &profile, &url, 800, 600)
        .await
        .expect("second open");

    assert!(manager.close("restartable").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn info_reaps_a_finished_browser_handler_instead_of_reporting_active() {
    require_chrome!();
    let (url, server) = local_page("CCCC browser exit status").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let key = "browser-exit-status";

    manager
        .open(key, &temp.path().join("profile"), &url, 800, 600)
        .await
        .expect("open");
    manager
        .sessions
        .lock()
        .await
        .get_mut(key)
        .expect("session")
        .handler
        .abort();
    tokio::task::yield_now().await;

    let status = manager.info(key).await;

    assert_eq!(status["active"], false);
    assert_eq!(status["state"], "failed");
    assert_eq!(status["error"]["code"], "browser_surface_process_exited");
    assert!(manager.sessions.lock().await.get(key).is_none());
    server.abort();
}

#[tokio::test]
async fn close_releases_key_for_a_different_profile() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let first_profile = temp.path().join("profile-1");
    let second_profile = temp.path().join("profile-2");

    manager
        .register_profile("space-provider::notebooklm", &first_profile)
        .await
        .expect("register first profile");
    assert!(
        !manager
            .close("space-provider::notebooklm")
            .await
            .expect("close inactive registration")
    );
    manager
        .register_profile("space-provider::notebooklm", &second_profile)
        .await
        .expect("register replacement profile");

    assert_eq!(
        manager
            .key_profiles
            .lock()
            .await
            .get("space-provider::notebooklm"),
        Some(&second_profile)
    );
}

#[tokio::test]
async fn inactive_stale_profile_is_replaced_for_the_same_key() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let first_profile = temp.path().join("profile-1");
    let second_profile = temp.path().join("profile-2");

    manager
        .register_profile("space-provider::notebooklm", &first_profile)
        .await
        .expect("register stale profile");
    manager
        .register_profile("space-provider::notebooklm", &second_profile)
        .await
        .expect("replace inactive stale profile");

    assert_eq!(
        manager
            .key_profiles
            .lock()
            .await
            .get("space-provider::notebooklm"),
        Some(&second_profile)
    );
}

#[tokio::test]
async fn failed_open_releases_profile_registration() {
    require_chrome!();
    let (url, server) = local_page("CCCC failed open cleanup").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let profile = temp.path().join("profile");
    let invalid_storage = json!({"cookies":"not-an-array"});

    manager
        .open_seeded(
            "failed-open",
            &profile,
            &url,
            800,
            600,
            Some(&invalid_storage),
        )
        .await
        .expect_err("invalid cookies should fail initialization");

    assert!(
        !manager
            .key_profiles
            .lock()
            .await
            .contains_key("failed-open")
    );
    server.abort();
}

#[tokio::test]
async fn open_and_close_share_one_profile_lifecycle_boundary() {
    require_chrome!();
    let (url, server) = local_page("CCCC open close race").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let profile = temp.path().join("profile");

    manager
        .open("race", &profile, &url, 800, 600)
        .await
        .expect("initial open");
    let (opened, closed) = tokio::join!(
        manager.open("race", &profile, &url, 800, 600),
        manager.close("race"),
    );

    opened.expect("racing open");
    closed.expect("racing close");
    let _ = manager.close("race").await;
    server.abort();
}

#[tokio::test]
async fn shutdown_closes_all_browser_processes() {
    require_chrome!();
    let (url, server) = local_page("CCCC shutdown browser").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let profile = temp.path().join("profile");

    manager
        .open("shutdown-test", &profile, &url, 800, 600)
        .await
        .expect("open");

    assert_eq!(manager.shutdown_all().await.expect("shutdown"), 1);
    assert!(manager.sessions.lock().await.is_empty());
    assert!(
        manager
            .open("shutdown-test", &profile, &url, 800, 600)
            .await
            .expect_err("open after shutdown must fail")
            .to_string()
            .contains("shutting down")
    );
    server.abort();
}

async fn local_page(body: &'static str) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("response");
        }
    });
    (format!("http://{address}"), server)
}

async fn page_with_stalled_subresource() -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = [0_u8; 2048];
                let Ok(read) = stream.read(&mut request).await else {
                    return;
                };
                if String::from_utf8_lossy(&request[..read]).starts_with("GET /never ") {
                    futures_util::future::pending::<()>().await;
                    return;
                }
                let body = "<!doctype html><html><body><h1>DOMContentLoaded</h1><img src='/never'></body></html>";
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (format!("http://{address}"), server)
}

#[tokio::test]
async fn restores_seeded_cookie_and_detects_real_auth_tokens() {
    require_chrome!();
    let (url, server) = local_page(
        "<!doctype html><script>globalThis.WIZ_global_data={SNlM0e:'csrf',FdrFJe:'session'}</script>",
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let seed = json!({"cookies":[{
        "name":"SID", "value":"present", "url":url, "path":"/",
        "secure":false, "httpOnly":false
    }]});
    manager
        .open_seeded(
            "notebooklm-test",
            &temp.path().join("profile"),
            &url,
            800,
            600,
            Some(&seed),
        )
        .await
        .expect("open seeded browser");
    let page = manager
        .sessions
        .lock()
        .await
        .get("notebooklm-test")
        .expect("browser session")
        .page
        .clone();
    let cookie: String = page
        .evaluate("document.cookie")
        .await
        .expect("evaluate cookie")
        .into_value()
        .expect("cookie string");
    assert!(cookie.contains("SID=present"));
    assert!(
        manager
            .notebooklm_auth_ready("notebooklm-test")
            .await
            .expect("auth probe")
    );
    assert!(manager.close("notebooklm-test").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn special_key_command_applies_native_input_behavior() {
    require_chrome!();
    let (url, server) = local_page(
        "<!doctype html><input id='email' autofocus value='waterbang@'><script>email.setSelectionRange(email.value.length,email.value.length)</script>",
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager
        .open(
            "keyboard-test",
            &temp.path().join("profile"),
            &url,
            800,
            600,
        )
        .await
        .expect("open browser");
    manager
        .sessions
        .lock()
        .await
        .get("keyboard-test")
        .expect("browser session")
        .page
        .evaluate("document.querySelector('#email').focus()")
        .await
        .expect("focus input");
    manager
        .command("keyboard-test", &json!({"t":"key","key":"Backspace"}))
        .await
        .expect("press backspace");
    let page = manager
        .sessions
        .lock()
        .await
        .get("keyboard-test")
        .expect("browser session")
        .page
        .clone();
    let value: String = page
        .evaluate("document.querySelector('#email').value")
        .await
        .expect("read input")
        .into_value()
        .expect("input value");
    assert_eq!(value, "waterbang");
    assert!(manager.close("keyboard-test").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn click_command_preserves_the_requested_mouse_button() {
    require_chrome!();
    let (url, server) = local_page(
        "<!doctype html><style>html,body{margin:0}#target{width:300px;height:300px}</style><div id='target'>target</div><script>target.addEventListener('contextmenu',event=>{event.preventDefault();document.body.dataset.button=String(event.button)})</script>",
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager
        .open(
            "mouse-button-test",
            &temp.path().join("profile"),
            &url,
            800,
            600,
        )
        .await
        .expect("open browser");

    manager
        .command(
            "mouse-button-test",
            &json!({"t":"click","x":100,"y":100,"button":"right"}),
        )
        .await
        .expect("right click");
    let page = manager
        .sessions
        .lock()
        .await
        .get("mouse-button-test")
        .expect("browser session")
        .page
        .clone();
    let button: String = page
        .evaluate("document.body.dataset.button || ''")
        .await
        .expect("read context-menu button")
        .into_value()
        .expect("button value");

    assert_eq!(button, "2");
    assert!(manager.close("mouse-button-test").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn core_interaction_commands_complete_a_real_page_journey() {
    require_chrome!();
    let (url, server) = local_page(
        "<!doctype html><style>html,body{margin:0}#input{position:absolute;left:20px;top:20px;width:240px;height:50px}</style><input id='input'><script>sessionStorage.loads=String(Number(sessionStorage.loads||0)+1)</script>",
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager
        .open(
            "navigate-test",
            &temp.path().join("profile"),
            &url,
            800,
            600,
        )
        .await
        .expect("open browser");
    let page = manager
        .sessions
        .lock()
        .await
        .get("navigate-test")
        .expect("browser session")
        .page
        .clone();
    let start_url = page
        .url()
        .await
        .expect("read start URL")
        .expect("start URL");

    manager
        .command(
            "navigate-test",
            &json!({"t":"click","x":100,"y":45,"button":"left"}),
        )
        .await
        .expect("focus input");
    manager
        .command("navigate-test", &json!({"t":"text","text":"hello"}))
        .await
        .expect("insert text");
    manager
        .command("navigate-test", &json!({"t":"key","key":"Backspace"}))
        .await
        .expect("press key");
    let value: String = page
        .evaluate("document.querySelector('#input').value")
        .await
        .expect("read input")
        .into_value()
        .expect("input value");
    assert_eq!(value, "hell");

    manager
        .command(
            "navigate-test",
            &json!({"t":"resize","width":960,"height":720}),
        )
        .await
        .expect("resize");
    let viewport: Value = page
        .evaluate("({width:window.innerWidth,height:window.innerHeight})")
        .await
        .expect("read viewport")
        .into_value()
        .expect("viewport");
    assert_eq!(viewport, json!({"width":960,"height":720}));

    let target = format!("{url}/next");

    manager
        .command("navigate-test", &json!({"t":"navigate","url":target}))
        .await
        .expect("navigate");
    let observed = page
        .url()
        .await
        .expect("read browser URL")
        .expect("browser URL");

    assert_eq!(observed, target);
    assert_eq!(manager.info("navigate-test").await["url"], target);
    manager
        .command("navigate-test", &json!({"t":"refresh"}))
        .await
        .expect("refresh");
    let loads: String = page
        .evaluate("sessionStorage.loads")
        .await
        .expect("read load count")
        .into_value()
        .expect("load count");
    assert_eq!(loads, "3");

    manager
        .command("navigate-test", &json!({"t":"back"}))
        .await
        .expect("back");
    assert_eq!(
        page.url().await.expect("read back URL"),
        Some(start_url.clone())
    );
    assert_eq!(manager.info("navigate-test").await["url"], start_url);
    assert!(manager.close("navigate-test").await.expect("close"));
    server.abort();
}

#[tokio::test]
async fn scroll_command_targets_the_nested_container_under_the_pointer() {
    require_chrome!();
    let (url, server) = local_page(
        "<!doctype html><style>html,body{margin:0;height:100%;overflow:hidden}#scroller{width:400px;height:300px;overflow:auto}#content{height:2000px}</style><div id='scroller'><div id='content'>scroll target</div></div>",
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager
        .open("scroll-test", &temp.path().join("profile"), &url, 800, 600)
        .await
        .expect("open browser");

    manager
        .command(
            "scroll-test",
            &json!({"t":"scroll","x":200,"y":150,"dx":0,"dy":240}),
        )
        .await
        .expect("dispatch wheel");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let page = manager
        .sessions
        .lock()
        .await
        .get("scroll-test")
        .expect("browser session")
        .page
        .clone();
    let scroll_top: f64 = page
        .evaluate("document.querySelector('#scroller').scrollTop")
        .await
        .expect("read nested scroll position")
        .into_value()
        .expect("numeric scroll position");

    assert!(
        scroll_top >= 200.0,
        "nested container did not scroll: {scroll_top}"
    );
    assert!(manager.close("scroll-test").await.expect("close"));
    server.abort();
}

fn chrome_available() -> bool {
    [
        "/opt/homebrew/bin/chromium",
        "/usr/bin/chromium",
        "/usr/bin/google-chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ]
    .iter()
    .any(|path| std::path::Path::new(path).is_file())
}

#[test]
fn classifies_only_group_owned_browser_sessions() {
    assert_eq!(session_group_id("g_one::presentation"), Some("g_one"));
    assert_eq!(session_group_id("web-model::g_two::actor"), Some("g_two"));
    assert_eq!(session_group_id("space-provider::notebooklm"), None);
    assert_eq!(
        session_actor("web-model::g_two::actor"),
        Some(("g_two", "actor"))
    );
    assert_eq!(session_actor("g_one::presentation"), None);
}

#[tokio::test]
async fn shared_web_model_operations_preserve_busy_drafts_and_serialize_manual_navigation() {
    require_chrome!();
    let (url,server)=local_page(r#"<!doctype html><html><body><form><textarea id="prompt-textarea" placeholder="Message">my unsent draft</textarea><button type="button" aria-label="Stop streaming">Stop</button></form></body></html>"#).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = std::sync::Arc::new(BrowserSurfaces::default());
    let profile = temp.path().join("profile");
    let (first, second) = tokio::join!(
        manager.ensure_open(SHARED_WEB_MODEL_KEY, &profile, &url, 800, 600),
        manager.ensure_open(SHARED_WEB_MODEL_KEY, &profile, &url, 800, 600),
    );
    assert_eq!(
        first.expect("first open")["started_at"],
        second.expect("reuse")["started_at"]
    );
    let page = manager
        .sessions
        .lock()
        .await
        .get(SHARED_WEB_MODEL_KEY)
        .expect("shared session")
        .page
        .clone();
    let result = manager
        .submit_prompt_with_attachment(
            SHARED_WEB_MODEL_KEY,
            &url,
            "member report",
            None,
            "test-busy",
        )
        .await
        .expect("submission result");
    assert!(matches!(
        result,
        prompt_submission::PromptSubmissionOutcome::Deferred(_)
    ));
    let draft: String = page
        .evaluate("document.querySelector('textarea').value")
        .await
        .expect("read composer")
        .into_value()
        .expect("composer string");
    assert_eq!(
        draft, "my unsent draft",
        "busy submission changed the user's draft"
    );
    let guard = manager.web_model_operation.lock().await;
    let other = std::sync::Arc::clone(&manager);
    let navigation = tokio::spawn(async move {
        other
            .command(SHARED_WEB_MODEL_KEY, &json!({"t":"text","text":"later"}))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !navigation.is_finished(),
        "manual interaction bypassed an in-flight browser transaction"
    );
    drop(guard);
    navigation.await.expect("join").expect("manual interaction");
    assert_eq!(manager.sessions.lock().await.len(), 1);
    manager
        .close(SHARED_WEB_MODEL_KEY)
        .await
        .expect("close shared browser");
    server.abort();
    eprintln!("shared browser launched, busy draft preserved, manual command serialized");
}

#[tokio::test]
async fn guest_composer_is_not_authenticated_delivery() {
    require_chrome!();
    let (url,server)=local_page(r#"<!doctype html><body><button data-mobile-auth-entry-action="login">登录</button><main><form><textarea id="prompt-textarea" placeholder="Message" style="width:500px;height:100px"></textarea><button type="button" aria-label="Send prompt" onclick="window.sent=(window.sent||0)+1">Send</button></form></main></body>"#).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = std::sync::Arc::new(BrowserSurfaces::default());
    manager
        .open("guest-check", &temp.path().join("profile"), &url, 800, 600)
        .await
        .expect("browser");
    let work = std::sync::Arc::clone(&manager);
    let outcome=tokio::spawn(async move {
        let readiness=work.prompt_readiness("guest-check").await.expect("readiness");
        assert_eq!(readiness["ready"],false,"guest composer was mistaken for a signed-in browser");
        assert_eq!(readiness["login_required"],true);
        let result=work.submit_prompt_with_attachment("guest-check",&url,"must not enter guest chat",None,"guest-report").await.expect("preflight");
        assert!(matches!(result,prompt_submission::PromptSubmissionOutcome::Deferred(_)),"guest report was submitted");
        let page=work.sessions.lock().await.get("guest-check").expect("session").page.clone();
        let untouched:bool=page.evaluate("!window.sent && document.querySelector('textarea').value === ''").await.expect("read").into_value().expect("bool");
        assert!(untouched,"guest preflight touched the composer or Send");
        page.evaluate("document.querySelector('[data-mobile-auth-entry-action]').remove();document.querySelector('main').insertAdjacentHTML('beforeend','<div data-message-author-role=assistant><button data-testid=login-button>Log in</button></div>')").await.expect("fixture login recovery");
        assert_eq!(work.prompt_readiness("guest-check").await.expect("recovered readiness")["ready"],true,"quoted login text inside a message blocked signed-in use");
        eprintln!("REAL_CHROME: guest login controls block readiness and send; message content is not authentication state");
    }).await;
    let _ = manager.close("guest-check").await;
    server.abort();
    outcome.expect("guest assertions");
}

#[tokio::test]
async fn cross_chat_delivery_does_not_navigate_away_from_a_draft() {
    require_chrome!();
    let (source,source_server)=local_page(r#"<!doctype html><textarea id="prompt-textarea" style="width:500px;height:100px">UNSENT_A_DRAFT</textarea>"#).await;
    let (destination,destination_server)=local_page(r#"<!doctype html><textarea id="prompt-textarea" style="width:500px;height:100px"></textarea><button aria-label="Send prompt" onclick="const t=document.querySelector('textarea');const p=document.createElement('div');p.dataset.messageAuthorRole='user';p.textContent=t.value;document.body.append(p);t.value='';window.sent=(window.sent||0)+1">Send</button>"#).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = std::sync::Arc::new(BrowserSurfaces::default());
    manager
        .open(
            "draft-routing",
            &temp.path().join("profile"),
            &source,
            800,
            600,
        )
        .await
        .expect("browser");
    let work = std::sync::Arc::clone(&manager);
    let outcome=tokio::spawn(async move {
        let result=work.submit_prompt_with_attachment("draft-routing",&destination,"REPORT_B",None,"cross-draft").await.expect("submission check");
        assert!(matches!(result,prompt_submission::PromptSubmissionOutcome::Deferred(_)),"B delivery navigated away from A's unsent draft");
        let page=work.sessions.lock().await.get("draft-routing").expect("session").page.clone();
        assert_eq!(page.url().await.expect("url").expect("URL"),format!("{source}/"));
        let draft:String=page.evaluate("document.querySelector('textarea').value").await.expect("draft").into_value().expect("text");
        assert_eq!(draft,"UNSENT_A_DRAFT");
        page.evaluate("document.querySelector('textarea').value=''").await.expect("user clears fixture draft");
        let resumed=work.submit_prompt_with_attachment("draft-routing",&destination,"REPORT_B",None,"cross-draft").await.expect("resumed submission");
        assert!(matches!(resumed,prompt_submission::PromptSubmissionOutcome::Verified(_)),"delivery did not recover when the draft was cleared");
        let count:u64=page.evaluate("window.sent||0").await.expect("send count").into_value().expect("count");
        assert_eq!(count,1);
        let again=work.submit_prompt_with_attachment("draft-routing",&destination,"REPORT_B",None,"cross-draft").await.expect("repeat observation");
        assert!(matches!(again,prompt_submission::PromptSubmissionOutcome::Verified(_)));
        let count:u64=page.evaluate("window.sent||0").await.expect("send count").into_value().expect("count");
        assert_eq!(count,1,"echo reconciliation submitted the same report again");
        eprintln!("REAL_CHROME: A draft blocks B navigation; clear -> one B submission -> duplicate observation does not resend");
    }).await;
    let _ = manager.close("draft-routing").await;
    source_server.abort();
    destination_server.abort();
    outcome.expect("cross-chat assertions");
}

#[tokio::test]
async fn submission_does_not_wait_for_background_intersection_observers() {
    require_chrome!();
    let (url,server)=local_page(r#"<!doctype html><html><body><form onsubmit="event.preventDefault();window.sent=(window.sent||0)+1;const e=document.createElement('div');e.dataset.messageAuthorRole='user';e.textContent=document.querySelector('textarea').value;document.body.append(e);document.querySelector('textarea').value=''"><textarea id="prompt-textarea" style="width:500px;height:100px"></textarea><button id="composer-submit-button" type="submit" aria-label="Send prompt">Send</button></form></body></html>"#).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager
        .open(
            "background-submit",
            &temp.path().join("profile"),
            &url,
            800,
            600,
        )
        .await
        .expect("browser");
    let page = manager
        .sessions
        .lock()
        .await
        .get("background-submit")
        .expect("session")
        .page
        .clone();
    // An inactive tab need not deliver IntersectionObserver callbacks. Model
    // that condition deterministically; never bring a user's window to front.
    page.evaluate("window.IntersectionObserver=class{observe(){} unobserve(){} disconnect(){}};")
        .await
        .expect("suspend observer fixture");
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        manager.submit_prompt_with_attachment(
            "background-submit",
            &url,
            "BACKGROUND_REPORT",
            None,
            "background-once",
        ),
    )
    .await;
    let count: u64 = page
        .evaluate("window.sent||0")
        .await
        .expect("send count")
        .into_value()
        .expect("count");
    let repeated = if outcome.as_ref().is_ok_and(|result| {
        matches!(
            result,
            Ok(prompt_submission::PromptSubmissionOutcome::Verified(_))
        )
    }) {
        Some(
            manager
                .submit_prompt_with_attachment(
                    "background-submit",
                    &url,
                    "BACKGROUND_REPORT",
                    None,
                    "background-once",
                )
                .await,
        )
    } else {
        None
    };
    let final_count: u64 = page
        .evaluate("window.sent||0")
        .await
        .expect("final count")
        .into_value()
        .expect("count");
    let _ = manager.close("background-submit").await;
    server.abort();
    assert!(
        matches!(
            outcome,
            Ok(Ok(prompt_submission::PromptSubmissionOutcome::Verified(_)))
        ),
        "background submission waited for a visual observer instead of invoking the checked Send control"
    );
    assert_eq!(count, 1);
    assert!(matches!(
        repeated,
        Some(Ok(prompt_submission::PromptSubmissionOutcome::Verified(_)))
    ));
    assert_eq!(
        final_count, 1,
        "an existing message echo was sent a second time"
    );
    eprintln!(
        "BACKGROUND_SUBMISSION: suspended visual observer, one verified send, duplicate observation does not resend"
    );
}

#[tokio::test]
async fn relay_idle_probe_requires_an_empty_non_generating_composer() {
    require_chrome!();
    let (url, server) = local_page(
        r#"<!doctype html><body><form><textarea id="prompt-textarea" placeholder="Message"></textarea><button type="button" aria-label="Send prompt">Send</button><button id="busy" type="button" aria-label="Stop streaming">Stop</button></form></body>"#,
    )
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager
        .open("relay-idle", &temp.path().join("profile"), &url, 800, 600)
        .await
        .expect("browser");
    assert!(
        !manager
            .relay_surface_idle("relay-idle")
            .await
            .expect("busy probe")
    );
    assert_eq!(
        manager
            .relay_surface_deferral("relay-idle")
            .await
            .expect("busy evidence")
            .expect("busy deferral")["submission_evidence"],
        "not_sent_chat_busy"
    );
    let page = manager
        .sessions
        .lock()
        .await
        .get("relay-idle")
        .expect("session")
        .page
        .clone();
    page.evaluate("document.querySelector('#busy').remove();document.querySelector('textarea').value='HUMAN DRAFT'")
        .await
        .expect("draft fixture");
    assert!(
        !manager
            .relay_surface_idle("relay-idle")
            .await
            .expect("draft probe")
    );
    assert_eq!(
        manager
            .relay_surface_deferral("relay-idle")
            .await
            .expect("draft evidence")
            .expect("draft deferral")["submission_evidence"],
        "not_sent_composer_occupied"
    );
    page.evaluate("document.querySelector('textarea').value=''")
        .await
        .expect("clear fixture");
    assert!(
        manager
            .relay_surface_idle("relay-idle")
            .await
            .expect("idle probe")
    );
    assert!(
        manager
            .relay_surface_deferral("relay-idle")
            .await
            .expect("idle evidence")
            .is_none()
    );
    manager.close("relay-idle").await.expect("close browser");
    server.abort();
}
