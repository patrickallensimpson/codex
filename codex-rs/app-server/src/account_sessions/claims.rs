use base64::Engine;
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct AccessTokenClaims {
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: AccessTokenProfileClaims,
}

#[derive(Default, Deserialize)]
struct AccessTokenProfileClaims {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    picture: Option<String>,
}

pub(super) fn profile_from_access_token(access_token: &str) -> (Option<String>, Option<String>) {
    let Some(payload) = access_token.split('.').nth(1) else {
        return (None, None);
    };
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|payload| serde_json::from_slice::<AccessTokenClaims>(&payload).ok())
        .unwrap_or_default();
    (
        claims.profile.name,
        claims.profile.picture.or(claims.profile.image),
    )
}
