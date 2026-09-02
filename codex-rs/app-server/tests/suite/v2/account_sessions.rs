use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_mock_responses_config_toml_with_chatgpt_base_url;
use app_test_support::write_models_cache;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::load_auth_dot_json;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::MockServer;

const FIRST_ACCOUNT_ID: &str = "123e4567-e89b-42d3-a456-426614174101";
const SECOND_ACCOUNT_ID: &str = "123e4567-e89b-42d3-a456-426614174102";

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
