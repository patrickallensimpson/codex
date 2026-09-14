use chrono::Utc;
use codex_app_server_protocol::AccountSession;
use codex_app_server_protocol::AccountSessionWorkspace;
use codex_app_server_protocol::AccountSessionWorkspaceKind;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_backend_client::AccountEntry;
use codex_backend_client::Client as BackendClient;
use codex_config::types::AuthCredentialsStoreMode;
use codex_http_client::HttpClientFactory;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::load_auth_dot_json;
use codex_login::logout;
use codex_login::logout_with_revoke;
use codex_login::save_auth;
use serde::Deserialize;
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

mod claims;

use claims::profile_from_access_token;

const ACCOUNT_SESSIONS_FILE: &str = "account-sessions.json";
const ACCOUNT_SESSIONS_DIR: &str = "account-sessions";

pub(crate) struct AccountSessionsStore<'a> {
    codex_home: &'a Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    auth_keyring_backend_kind: AuthKeyringBackendKind,
    chatgpt_base_url: &'a str,
    auth_route_config: AuthRouteConfig,
    http_client_factory: HttpClientFactory,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccountSessions {
    active_session_id: Option<String>,
    sessions: Vec<StoredAccountSession>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccountSession {
    session_id: String,
    email: Option<String>,
    user_id: Option<String>,
    display_name: Option<String>,
    image_url: Option<String>,
    last_used_at: i64,
    selected_workspace_account_id: Option<String>,
    workspaces: Vec<AccountSessionWorkspace>,
}

impl<'a> AccountSessionsStore<'a> {
    pub(crate) fn new(
        codex_home: &'a Path,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        auth_keyring_backend_kind: AuthKeyringBackendKind,
        chatgpt_base_url: &'a str,
        auth_route_config: AuthRouteConfig,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            codex_home,
            auth_credentials_store_mode,
            auth_keyring_backend_kind,
            chatgpt_base_url,
            auth_route_config,
            http_client_factory,
        }
    }

    pub(crate) async fn add(
        &self,
        switch_to_added_account: bool,
    ) -> std::io::Result<AccountSessionsResponse> {
        self.sync_active_auth()?;
        let mut stored = self.load()?;
        let auth_json = self
            .load_active_auth()?
            .ok_or_else(|| std::io::Error::other("No active ChatGPT auth session to add"))?;
        let mut session = Self::session_from_auth_json(&auth_json)
            .ok_or_else(|| std::io::Error::other("No active ChatGPT auth session to add"))?;

        let existing_index = stored
            .sessions
            .iter()
            .position(|saved| Self::same_identity(saved, &auth_json));
        if let Some(index) = existing_index {
            session
                .session_id
                .clone_from(&stored.sessions[index].session_id);
        }
        self.save_session_auth(&session.session_id, &auth_json)?;
        self.refresh_workspace_metadata(&mut session).await;

        let added_session_id = session.session_id.clone();
        if let Some(index) = existing_index {
            stored.sessions[index] = session;
        } else {
            stored.sessions.push(session);
        }

        if switch_to_added_account || stored.active_session_id.is_none() {
            stored.active_session_id = Some(added_session_id);
        } else if let Some(active_session_id) = stored.active_session_id.as_deref() {
            let active_auth = self.load_session_auth(active_session_id)?.ok_or_else(|| {
                std::io::Error::other("Saved ChatGPT account session credentials not found")
            })?;
            self.save_active_auth(&active_auth)?;
        }

        self.save(&stored)?;
        Ok(Self::response(stored))
    }

    pub(crate) async fn list(
        &self,
        refresh_workspace_metadata: bool,
    ) -> std::io::Result<AccountSessionsResponse> {
        self.sync_active_auth()?;
        let mut stored = self.load()?;
        if refresh_workspace_metadata {
            for session in &mut stored.sessions {
                self.refresh_workspace_metadata(session).await;
            }
            self.save(&stored)?;
        }
        Ok(Self::response(stored))
    }

    pub(crate) async fn logout(
        &self,
        session_id: &str,
    ) -> std::io::Result<AccountSessionsResponse> {
        self.sync_active_auth()?;
        let mut stored = self.load()?;
        let index = stored
            .sessions
            .iter()
            .position(|session| session.session_id == session_id)
            .ok_or_else(|| std::io::Error::other("Saved ChatGPT account session not found"))?;
        let removed = stored.sessions.remove(index);

        if stored.active_session_id.as_deref() == Some(session_id) {
            let newest = stored
                .sessions
                .iter()
                .max_by_key(|session| session.last_used_at);
            stored.active_session_id = newest.map(|session| session.session_id.clone());
            match newest {
                Some(session) => {
                    let auth = self
                        .load_session_auth(&session.session_id)?
                        .ok_or_else(|| {
                            std::io::Error::other(
                                "Saved ChatGPT account session credentials not found",
                            )
                        })?;
                    self.save_active_auth(&auth)?;
                }
                None => {
                    logout(
                        self.codex_home,
                        self.auth_credentials_store_mode,
                        self.auth_keyring_backend_kind,
                    )?;
                }
            }
        }

        self.save(&stored)?;
        let session_home = self.session_home(&removed.session_id)?;
        if let Err(err) = logout_with_revoke(
            &session_home,
            self.auth_credentials_store_mode,
            self.auth_keyring_backend_kind,
            &self.auth_route_config,
        )
        .await
        {
            tracing::warn!("failed to revoke saved account session during logout: {err}");
        }
        Ok(Self::response(stored))
    }

    pub(crate) async fn switch(
        &self,
        session_id: &str,
        account_id: &str,
    ) -> std::io::Result<AccountSessionsResponse> {
        self.sync_active_auth()?;
        let mut stored = self.load()?;
        let index = stored
            .sessions
            .iter()
            .position(|session| session.session_id == session_id)
            .ok_or_else(|| std::io::Error::other("Saved ChatGPT account session not found"))?;
        let selected_known = stored.sessions[index]
            .workspaces
            .iter()
            .any(|workspace| workspace.account_id == account_id);
        let mut auth_json = self.load_session_auth(session_id)?.ok_or_else(|| {
            std::io::Error::other("Saved ChatGPT account session credentials not found")
        })?;
        let current_account_id = Self::selected_account_id(&auth_json);

        if current_account_id.as_deref() != Some(account_id) {
            if !selected_known && !stored.sessions[index].workspaces.is_empty() {
                return Err(std::io::Error::other(
                    "Requested workspace does not belong to the saved account session",
                ));
            }
            let auth_manager = self.session_auth_manager(session_id).await?;
            let auth = auth_manager
                .auth()
                .await
                .ok_or_else(|| std::io::Error::other("Saved ChatGPT account session is invalid"))?;
            // auth() may rotate and persist OAuth credentials. Apply the workspace
            // response to that record so an omitted refresh token keeps the new one.
            auth_json = self.load_session_auth(session_id)?.ok_or_else(|| {
                std::io::Error::other("Saved ChatGPT account session credentials not found")
            })?;
            let client = BackendClient::from_auth(
                self.chatgpt_base_url,
                &auth,
                self.http_client_factory.clone(),
            );
            let replacement = client
                .switch_workspace_token(account_id)
                .await
                .map_err(std::io::Error::other)?;
            let tokens = auth_json.tokens.as_mut().ok_or_else(|| {
                std::io::Error::other("Saved ChatGPT account session has no tokens")
            })?;
            tokens.access_token = replacement.access_token;
            if let Some(refresh_token) = replacement.refresh_token {
                tokens.refresh_token = refresh_token;
            }
            tokens.account_id = Some(account_id.to_string());
            auth_json.last_refresh = Some(Utc::now());
        }

        let session = &mut stored.sessions[index];
        session.selected_workspace_account_id = Some(account_id.to_string());
        session.last_used_at = Utc::now().timestamp();
        stored.active_session_id = Some(session_id.to_string());
        self.save_session_auth(session_id, &auth_json)?;
        self.save_active_auth(&auth_json)?;
        self.save(&stored)?;
        Ok(Self::response(stored))
    }

    pub(crate) fn sync_active_auth(&self) -> std::io::Result<()> {
        let mut stored = self.load()?;
        let Some(active_session_id) = stored.active_session_id.clone() else {
            return Ok(());
        };
        let Some(auth_json) = self.load_active_auth()? else {
            return Ok(());
        };
        let Some(session) = stored
            .sessions
            .iter_mut()
            .find(|session| session.session_id == active_session_id)
        else {
            return Ok(());
        };
        if !Self::same_identity(session, &auth_json) {
            return Ok(());
        }

        let selected_workspace_account_id = Self::selected_account_id(&auth_json);
        session.selected_workspace_account_id = selected_workspace_account_id;
        self.save_session_auth(&active_session_id, &auth_json)?;
        self.save(&stored)
    }

    async fn refresh_workspace_metadata(&self, session: &mut StoredAccountSession) {
        let Ok(auth_manager) = self.session_auth_manager(&session.session_id).await else {
            return;
        };
        let Some(auth) = auth_manager.auth().await else {
            return;
        };
        let client = BackendClient::from_auth(
            self.chatgpt_base_url,
            &auth,
            self.http_client_factory.clone(),
        );
        let Ok(accounts) = client.get_accounts_check().await else {
            return;
        };
        session.selected_workspace_account_id = session
            .selected_workspace_account_id
            .clone()
            .or(accounts.default_account_id)
            .or_else(|| accounts.account_ordering.first().cloned());
        session.workspaces = accounts
            .accounts
            .into_iter()
            .map(Self::workspace_from_account)
            .collect();
    }

    async fn session_auth_manager(&self, session_id: &str) -> std::io::Result<AuthManager> {
        Ok(AuthManager::new(
            self.session_home(session_id)?,
            /*enable_codex_api_key_env*/ false,
            self.auth_credentials_store_mode,
            /*forced_chatgpt_workspace_id*/ None,
            Some(self.chatgpt_base_url.to_string()),
            self.auth_keyring_backend_kind,
            self.auth_route_config.clone(),
        )
        .await)
    }

    fn load(&self) -> std::io::Result<StoredAccountSessions> {
        match std::fs::read_to_string(self.path()) {
            Ok(payload) => serde_json::from_str(&payload).map_err(std::io::Error::other),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => self.bootstrap(),
            Err(err) => Err(err),
        }
    }

    fn bootstrap(&self) -> std::io::Result<StoredAccountSessions> {
        let Some(auth_json) = self.load_active_auth()? else {
            return Ok(StoredAccountSessions::default());
        };
        let Some(session) = Self::session_from_auth_json(&auth_json) else {
            return Ok(StoredAccountSessions::default());
        };
        self.save_session_auth(&session.session_id, &auth_json)?;
        let stored = StoredAccountSessions {
            active_session_id: Some(session.session_id.clone()),
            sessions: vec![session],
        };
        self.save(&stored)?;
        Ok(stored)
    }

    fn save(&self, sessions: &StoredAccountSessions) -> std::io::Result<()> {
        let path = self.path();
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("account sessions path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(serde_json::to_string_pretty(sessions)?.as_bytes())?;
        temporary.flush()?;
        temporary.persist(path).map_err(|err| err.error)?;
        Ok(())
    }

    fn load_active_auth(&self) -> std::io::Result<Option<AuthDotJson>> {
        load_auth_dot_json(
            self.codex_home,
            self.auth_credentials_store_mode,
            self.auth_keyring_backend_kind,
        )
    }

    fn save_active_auth(&self, auth_json: &AuthDotJson) -> std::io::Result<()> {
        save_auth(
            self.codex_home,
            auth_json,
            self.auth_credentials_store_mode,
            self.auth_keyring_backend_kind,
        )
    }

    fn load_session_auth(&self, session_id: &str) -> std::io::Result<Option<AuthDotJson>> {
        load_auth_dot_json(
            &self.session_home(session_id)?,
            self.auth_credentials_store_mode,
            self.auth_keyring_backend_kind,
        )
    }

    fn save_session_auth(&self, session_id: &str, auth_json: &AuthDotJson) -> std::io::Result<()> {
        save_auth(
            &self.session_home(session_id)?,
            auth_json,
            self.auth_credentials_store_mode,
            self.auth_keyring_backend_kind,
        )
    }

    fn session_home(&self, session_id: &str) -> std::io::Result<PathBuf> {
        Uuid::parse_str(session_id)
            .map_err(|_| std::io::Error::other("Invalid saved account session id"))?;
        let path = self.codex_home.join(ACCOUNT_SESSIONS_DIR).join(session_id);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn session_from_auth_json(auth_json: &AuthDotJson) -> Option<StoredAccountSession> {
        let tokens = auth_json.tokens.as_ref()?;
        let (display_name, image_url) = profile_from_access_token(&tokens.access_token);
        let selected_workspace_account_id = Self::selected_account_id(auth_json);
        let workspaces = selected_workspace_account_id
            .as_ref()
            .map(|account_id| {
                vec![AccountSessionWorkspace {
                    account_id: account_id.clone(),
                    name: None,
                    image_url: None,
                    kind: None,
                }]
            })
            .unwrap_or_default();
        Some(StoredAccountSession {
            session_id: Uuid::now_v7().to_string(),
            email: tokens.id_token.email.clone(),
            user_id: tokens.id_token.chatgpt_user_id.clone(),
            display_name,
            image_url,
            last_used_at: Utc::now().timestamp(),
            selected_workspace_account_id,
            workspaces,
        })
    }

    fn selected_account_id(auth_json: &AuthDotJson) -> Option<String> {
        let tokens = auth_json.tokens.as_ref()?;
        tokens
            .account_id
            .clone()
            .or_else(|| tokens.id_token.chatgpt_account_id.clone())
    }

    fn same_identity(session: &StoredAccountSession, auth_json: &AuthDotJson) -> bool {
        let Some(tokens) = auth_json.tokens.as_ref() else {
            return false;
        };
        match (&session.user_id, &tokens.id_token.chatgpt_user_id) {
            (Some(saved), Some(active)) => saved == active,
            _ => session
                .email
                .as_ref()
                .zip(tokens.id_token.email.as_ref())
                .is_some_and(|(saved, active)| saved == active),
        }
    }

    fn workspace_from_account(account: AccountEntry) -> AccountSessionWorkspace {
        let kind = match account.structure.as_str() {
            "personal" => Some(AccountSessionWorkspaceKind::Personal),
            "workspace" => Some(AccountSessionWorkspaceKind::Workspace),
            _ => None,
        };
        AccountSessionWorkspace {
            account_id: account.id,
            name: account.name,
            image_url: account.profile_picture_url,
            kind,
        }
    }

    fn response(stored: StoredAccountSessions) -> AccountSessionsResponse {
        let active_session_id = stored.active_session_id;
        let mut sessions = stored
            .sessions
            .into_iter()
            .map(|session| AccountSession {
                is_active: Some(&session.session_id) == active_session_id.as_ref(),
                session_id: session.session_id,
                email: session.email,
                user_id: session.user_id,
                display_name: session.display_name,
                image_url: session.image_url,
                last_used_at: session.last_used_at,
                selected_workspace_account_id: session.selected_workspace_account_id,
                workspaces: session.workspaces,
            })
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_used_at));
        AccountSessionsResponse {
            active_session_id,
            sessions,
        }
    }

    fn path(&self) -> PathBuf {
        self.codex_home.join(ACCOUNT_SESSIONS_FILE)
    }
}
