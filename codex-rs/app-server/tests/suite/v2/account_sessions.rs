use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_mock_responses_config_toml_with_chatgpt_base_url;
use app_test_support::write_models_cache;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::path_regex;

const FIRST_ACCOUNT_ID: &str = "123e4567-e89b-42d3-a456-426614174101";
const SECOND_ACCOUNT_ID: &str = "123e4567-e89b-42d3-a456-426614174102";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn account_sessions_reject_mutations_until_active_turn_completes() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (release, gate) = tokio::sync::oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(gate),
        body: sse(vec![
            ev_assistant_message("message", "Done"),
            ev_completed("response"),
        ]),
    }]])
    .await;
    MockResponsesConfig::new(server.uri()).write(codex_home.path())?;
    write_models_cache(codex_home.path()).await?;
    write_auth(
        codex_home.path(),
        "first-access-token",
        "first-refresh-token",
        FIRST_ACCOUNT_ID,
        "first-user",
        "first@example.com",
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;
    let list_id = app_server
        .send_raw_request(
            "accountSession/list",
            Some(json!({ "refreshWorkspaceMetadata": false })),
        )
        .await?;
    let listed: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(list_id)).await??;
    let session_id = listed.active_session_id.expect("saved account");
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    let turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Wait for release".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(READ_TIMEOUT, app_server.read_response(turn_id)).await??;
    timeout(READ_TIMEOUT, server.wait_for_request_count(1)).await?;
    // A gated model response makes the active-turn boundary deterministic.
    for (method, params) in [
        (
            "accountSession/switch",
            json!({ "sessionId": session_id, "accountId": FIRST_ACCOUNT_ID }),
        ),
        (
            "accountSession/add",
            json!({ "switchToAddedAccount": false }),
        ),
        ("accountSession/logout", json!({ "sessionId": session_id })),
    ] {
        let request_id = app_server.send_raw_request(method, Some(params)).await?;
        let error = timeout(
            READ_TIMEOUT,
            app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.error.code, -32600);
        assert_eq!(
            error.error.message,
            "account sessions cannot change while a turn is active"
        );
    }
    release.send(()).expect("release model response");
    let completed: TurnCompletedNotification =
        timeout(READ_TIMEOUT, app_server.read_notification("turn/completed")).await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let switch_id = app_server
        .send_raw_request(
            "accountSession/switch",
            Some(json!({ "sessionId": session_id, "accountId": FIRST_ACCOUNT_ID })),
        )
        .await?;
    let switched: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(switch_id)).await??;
    assert_eq!(switched.active_session_id, Some(session_id));
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn account_sessions_existing_thread_uses_selected_account_on_next_turn() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    MockResponsesConfig::new(&server.uri())
        .with_root_config(&format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}\"",
            server.uri()
        ))
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = false")
        .with_extra_config("[features]\nenable_request_compression = false")
        .write(codex_home.path())?;
    write_models_cache(codex_home.path()).await?;
    write_auth(
        codex_home.path(),
        "first-access-token",
        "first-refresh-token",
        FIRST_ACCOUNT_ID,
        "first-user",
        "first@example.com",
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;
    let list_id = app_server
        .send_raw_request(
            "accountSession/list",
            Some(json!({ "refreshWorkspaceMetadata": false })),
        )
        .await?;
    let first: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(list_id)).await??;
    let first_session_id = first.active_session_id.expect("first saved account");
    write_auth(
        codex_home.path(),
        "second-access-token",
        "second-refresh-token",
        SECOND_ACCOUNT_ID,
        "second-user",
        "second@example.com",
    )?;
    let add_id = app_server
        .send_raw_request(
            "accountSession/add",
            Some(json!({ "switchToAddedAccount": false })),
        )
        .await?;
    let added: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(add_id)).await??;
    assert_eq!(added.active_session_id, Some(first_session_id.clone()));
    let second_session_id = added
        .sessions
        .iter()
        .find(|session| session.session_id != first_session_id)
        .expect("second saved account")
        .session_id
        .clone();
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;

    for (session_id, account_id, prompt) in [
        (
            first_session_id,
            FIRST_ACCOUNT_ID,
            "Remember the shared conversation marker",
        ),
        (
            second_session_id,
            SECOND_ACCOUNT_ID,
            "Continue the same conversation",
        ),
    ] {
        let switch_id = app_server
            .send_raw_request(
                "accountSession/switch",
                Some(json!({
                    "sessionId": session_id, "accountId": account_id
                })),
            )
            .await?;
        let switched: AccountSessionsResponse =
            timeout(READ_TIMEOUT, app_server.read_response(switch_id)).await??;
        assert_eq!(switched.active_session_id, Some(session_id));
        let completed = timeout(
            READ_TIMEOUT,
            app_server.start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            }),
        )
        .await??;
        assert_eq!(completed.thread_id, thread.id);
        assert_eq!(completed.turn.status, TurnStatus::Completed);
    }

    let requests = server.received_requests().await.expect("captured requests");
    let model_requests = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect::<Vec<_>>();
    let identities = model_requests
        .iter()
        .map(|request| {
            (
                request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                request
                    .headers
                    .get("chatgpt-account-id")
                    .and_then(|value| value.to_str().ok()),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        identities,
        vec![
            (Some("Bearer first-access-token"), Some(FIRST_ACCOUNT_ID)),
            (Some("Bearer second-access-token"), Some(SECOND_ACCOUNT_ID)),
        ]
    );
    let second_body: serde_json::Value = model_requests[1].body_json()?;
    let history = second_body["input"].to_string();
    assert!(history.contains("Remember the shared conversation marker"));
    assert!(history.contains("Continue the same conversation"));
    Ok(())
}

#[tokio::test]
async fn account_sessions_allow_overlapping_turn_starts_on_different_threads() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    write_models_cache(codex_home.path()).await?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let mut threads = Vec::new();
    for _ in 0..8 {
        threads.push(
            app_server
                .start_thread(ThreadStartParams::default())
                .await?
                .thread
                .id,
        );
    }

    // Keep requests outstanding together: awaiting each response here would hide
    // the regression where one thread's admission blocks another thread.
    let mut requests = Vec::new();
    for thread_id in &threads {
        requests.push(
            app_server
                .send_turn_start_request(TurnStartParams {
                    thread_id: thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: "Say done".to_string(),
                        text_elements: Vec::new(),
                    }],
                    ..Default::default()
                })
                .await?,
        );
    }
    let mut expected = BTreeSet::new();
    for (thread_id, request_id) in threads.into_iter().zip(requests) {
        let response: TurnStartResponse =
            timeout(READ_TIMEOUT, app_server.read_response(request_id)).await??;
        expected.insert((thread_id, response.turn.id));
    }
    let mut completed = BTreeSet::new();
    for _ in 0..expected.len() {
        let note: TurnCompletedNotification =
            timeout(READ_TIMEOUT, app_server.read_notification("turn/completed")).await??;
        assert_eq!(note.turn.status, TurnStatus::Completed);
        completed.insert((note.thread_id, note.turn.id));
    }
    assert_eq!(completed, expected);
    Ok(())
}

#[test_case(None; "preserves_rotated_refresh_token_when_workspace_response_omits_it")]
#[test_case(Some("workspace-refresh-token"); "uses_workspace_refresh_token_when_supplied")]
#[tokio::test]
async fn account_sessions_workspace_switch_preserves_refreshed_credentials(
    workspace_refresh_token: Option<&str>,
) -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        codex_home.path(),
        &server.uri(),
        &server.uri(),
    )?;
    write_models_cache(codex_home.path()).await?;
    write_auth(
        codex_home.path(),
        "first-access-token",
        "old-refresh-token",
        FIRST_ACCOUNT_ID,
        "first-user",
        "first@example.com",
    )?;
    Mock::given(method("GET"))
        .and(path_regex(".*/accounts/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [
                { "id": FIRST_ACCOUNT_ID, "structure": "personal" },
                { "id": SECOND_ACCOUNT_ID, "structure": "workspace" }
            ],
            "account_ordering": [FIRST_ACCOUNT_ID, SECOND_ACCOUNT_ID],
            "default_account_id": FIRST_ACCOUNT_ID
        })))
        .mount(&server)
        .await;
    let refresh_url = format!("{}/oauth/token", server.uri());
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (
                REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
                Some(refresh_url.as_str()),
            ),
        ])
        .build_initialized()
        .await?;
    let list_id = app_server
        .send_raw_request(
            "accountSession/list",
            Some(json!({ "refreshWorkspaceMetadata": true })),
        )
        .await?;
    let listed: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(list_id)).await??;
    let saved_session_id = listed.sessions[0].session_id.clone();
    assert_eq!(listed.sessions[0].workspaces.len(), 2);

    // Leave the first login inactive, then age its saved credentials. This keeps
    // startup and metadata discovery from refreshing them before the switch.
    write_auth(
        codex_home.path(),
        "second-access-token",
        "second-refresh-token",
        SECOND_ACCOUNT_ID,
        "second-user",
        "second@example.com",
    )?;
    let add_id = app_server
        .send_raw_request(
            "accountSession/add",
            Some(json!({ "switchToAddedAccount": true })),
        )
        .await?;
    let _: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(add_id)).await??;
    let saved_home = codex_home
        .path()
        .join("account-sessions")
        .join(&saved_session_id);
    let mut expected = load_auth_dot_json(
        &saved_home,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("saved credentials");
    expected.last_refresh = Some(chrono::Utc::now() - chrono::Duration::days(30));
    save_auth(
        &saved_home,
        &expected,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "refreshed-access-token",
            "refresh_token": "rotated-refresh-token"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut replacement = json!({ "access_token": "workspace-access-token" });
    if let Some(refresh_token) = workspace_refresh_token {
        replacement["refresh_token"] = json!(refresh_token);
    }
    Mock::given(method("POST"))
        .and(path("/accounts/switch-workspace-token"))
        .and(header("authorization", "Bearer refreshed-access-token"))
        .and(body_json(json!({ "workspace_id": SECOND_ACCOUNT_ID })))
        .respond_with(ResponseTemplate::new(200).set_body_json(replacement))
        .expect(1)
        .mount(&server)
        .await;
    let switch_id = app_server
        .send_raw_request(
            "accountSession/switch",
            Some(json!({
                "sessionId": saved_session_id, "accountId": SECOND_ACCOUNT_ID
            })),
        )
        .await?;
    let switched: AccountSessionsResponse =
        timeout(READ_TIMEOUT, app_server.read_response(switch_id)).await??;
    assert_eq!(switched.active_session_id, Some(saved_session_id));

    let tokens = expected.tokens.as_mut().expect("saved tokens");
    tokens.access_token = "workspace-access-token".to_string();
    tokens.refresh_token = workspace_refresh_token
        .unwrap_or("rotated-refresh-token")
        .to_string();
    tokens.account_id = Some(SECOND_ACCOUNT_ID.to_string());
    for home in [saved_home.as_path(), codex_home.path()] {
        let mut actual = load_auth_dot_json(
            home,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?
        .expect("switched credentials");
        assert!(actual.last_refresh > expected.last_refresh);
        // Timestamps are generated by the server; compare the complete remaining
        // auth record, including the refresh token and selected workspace.
        actual.last_refresh = expected.last_refresh;
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[tokio::test]
async fn account_sessions_share_codex_home_and_isolate_credentials() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mock_server = MockServer::start().await;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        codex_home.path(),
        &mock_server.uri(),
        &mock_server.uri(),
    )?;
    write_models_cache(codex_home.path()).await?;
    write_auth(
        codex_home.path(),
        "first-access-token",
        "first-refresh-token",
        FIRST_ACCOUNT_ID,
        "first-user",
        "first@example.com",
    )?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;

    let list_id = app_server
        .send_raw_request(
            "accountSession/list",
            Some(json!({ "refreshWorkspaceMetadata": false })),
        )
        .await?;
    let first_list: AccountSessionsResponse = app_server.read_response(list_id).await?;
    assert_eq!(first_list.sessions.len(), 1);
    let first_session_id = first_list.sessions[0].session_id.clone();

    write_auth(
        codex_home.path(),
        "second-access-token",
        "second-refresh-token",
        SECOND_ACCOUNT_ID,
        "second-user",
        "second@example.com",
    )?;
    let add_id = app_server
        .send_raw_request(
            "accountSession/add",
            Some(json!({ "switchToAddedAccount": true })),
        )
        .await?;
    let added: AccountSessionsResponse = app_server.read_response(add_id).await?;
    assert_eq!(added.sessions.len(), 2);
    assert_eq!(
        added
            .sessions
            .iter()
            .find(|session| session.is_active)
            .and_then(|session| session.email.as_deref()),
        Some("second@example.com")
    );

    let switch_id = app_server
        .send_raw_request(
            "accountSession/switch",
            Some(json!({
                "sessionId": first_session_id,
                "accountId": FIRST_ACCOUNT_ID,
            })),
        )
        .await?;
    let switched: AccountSessionsResponse = app_server.read_response(switch_id).await?;
    assert_eq!(
        switched
            .sessions
            .iter()
            .find(|session| session.is_active)
            .and_then(|session| session.email.as_deref()),
        Some("first@example.com")
    );

    let active_auth = load_auth_dot_json(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("active auth");
    assert_eq!(
        active_auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.account_id.as_deref()),
        Some(FIRST_ACCOUNT_ID)
    );

    let metadata = std::fs::read_to_string(codex_home.path().join("account-sessions.json"))?;
    assert!(!metadata.contains("first-access-token"));
    assert!(!metadata.contains("second-access-token"));
    assert!(codex_home.path().join("account-sessions").is_dir());
    Ok(())
}

fn write_auth(
    codex_home: &std::path::Path,
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
    user_id: &str,
    email: &str,
) -> Result<()> {
    write_chatgpt_auth(
        codex_home,
        ChatGptAuthFixture::new(access_token)
            .refresh_token(refresh_token)
            .account_id(account_id)
            .chatgpt_account_id(account_id)
            .chatgpt_user_id(user_id)
            .email(email),
        AuthCredentialsStoreMode::File,
    )
}
