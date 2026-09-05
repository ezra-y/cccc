mod auth_support;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cccc_core::{GroupStore, HomeLayout, web_model_connectors};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("JSON response")
}

#[tokio::test]
async fn local_binding_endpoint_is_group_specific_authenticated_and_redacts_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("binding test", "").expect("group");
    let mut actor = cccc_contracts::Actor::new("web-lead");
    actor.runtime = cccc_contracts::ActorRuntime::WebModel;
    cccc_core::actors::add(&mut group, actor).expect("actor");
    store.save(&group).expect("save");
    let (connector, _) =
        web_model_connectors::create(&home, &group.group_id, "web-lead", "chatgpt", "")
            .expect("connector");
    let id = connector["connector_id"].as_str().expect("id");
    let url = format!("/api/v1/web-model/connectors/{id}/binding");
    let app = auth_support::authenticated_app(home.clone());
    let request = || {
        Request::post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request")
    };
    let unauthorized = cccc_web::app(home.clone())
        .oneshot(request())
        .await
        .expect("unauthorized response");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .clone()
        .oneshot(request())
        .await
        .expect("binding response");
    assert_eq!(response.status(), StatusCode::OK);
    let response = json_body(response).await;
    assert_eq!(response["result"]["group_id"], group.group_id);
    assert_eq!(response["result"]["actor_id"], "web-lead");
    assert_eq!(response["result"]["session_bound"], false);
    let code = response["result"]["code"].as_str().expect("fresh code");
    web_model_connectors::bind_session(&home, id, code, "test-conversation").expect("bind");
    let list = app
        .oneshot(
            Request::get("/api/v1/web-model/connectors")
                .body(Body::empty())
                .expect("list request"),
        )
        .await
        .expect("list response");
    let list = json_body(list).await;
    let item = &list["result"]["connectors"][0];
    assert_eq!(item["session_bound"], true);
    for name in ["session_hash", "previous_session_hash", "binding_code_hash"] {
        assert!(item.get(name).is_none());
    }
    assert!(!list.to_string().contains("test-conversation"));
    assert!(!list.to_string().contains(code));
    let exhibit = auth_support::authenticated_app_with_mode(home, cccc_web::WebMode::Exhibit)
        .oneshot(request())
        .await
        .expect("read-only response");
    assert_eq!(exhibit.status(), StatusCode::FORBIDDEN);
}
