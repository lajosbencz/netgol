use crate::config::{Config, OidcProvider};
use crate::user_store::{User, UserStore};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode as OAuth2Code, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse, TokenUrl,
};
use openidconnect::core::{
    CoreClient, CoreProviderMetadata, CoreResponseType, CoreUserInfoClaims,
};
use openidconnect::reqwest::async_http_client;
use openidconnect::{
    AuthenticationFlow, AuthorizationCode as OidcCode, IssuerUrl, Nonce, Scope,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const SESSION_COOKIE: &str = "session";
const SESSION_MAX_AGE_SECS: i64 = 30 * 24 * 3600;
const PENDING_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionClaims {
    /// SHA-256 hex of email — file index key.
    pub sub: String,
    pub uid: u32,
    pub exp: i64,
}

struct OAuthPending {
    pkce_verifier: PkceCodeVerifier,
    provider: String,
    created_at: Instant,
}

/// Per-provider runtime client. Built once at startup.
enum ProviderClient {
    Oidc(CoreClient),
    Plain { client: BasicClient, userinfo_url: String },
}

pub struct AuthState {
    pub cfg: Config,
    pub user_store: Arc<UserStore>,
    pub http: reqwest::Client,
    clients: HashMap<String, ProviderClient>,
    pending: Mutex<HashMap<String, OAuthPending>>,
}

impl AuthState {
    pub async fn new(cfg: Config, user_store: Arc<UserStore>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("netgol/0.3")
            .build()
            .expect("build reqwest client");
        let mut clients = HashMap::new();
        for (name, provider) in &cfg.oidc_providers {
            match build_client(name, provider, &cfg.base_url).await {
                Ok(c) => { clients.insert(name.clone(), c); }
                Err(e) => tracing::error!(provider = %name, err = %e, "provider client build failed"),
            }
        }
        Self { cfg, user_store, http, clients, pending: Mutex::new(HashMap::new()) }
    }

    fn client(&self, name: &str) -> Option<&ProviderClient> {
        self.clients.get(name)
    }
}

async fn build_client(name: &str, provider: &OidcProvider, base_url: &str) -> Result<ProviderClient, String> {
    let client_id = provider.client_id.as_deref()
        .ok_or_else(|| format!("client_id missing (set NETGOL_OIDC_PROVIDERS__{name}__CLIENT_ID)", name = name.to_uppercase()))?;
    let client_secret = provider.client_secret.as_deref()
        .ok_or_else(|| format!("client_secret missing (set NETGOL_OIDC_PROVIDERS__{name}__CLIENT_SECRET)", name = name.to_uppercase()))?;

    let redirect = RedirectUrl::new(provider.redirect_uri(base_url, name))
        .map_err(|e| format!("bad redirect_uri: {e}"))?;

    if let Some(issuer) = &provider.issuer_url {
        let issuer_url = IssuerUrl::new(issuer.clone())
            .map_err(|e| format!("bad issuer_url: {e}"))?;
        let metadata = CoreProviderMetadata::discover_async(issuer_url, async_http_client)
            .await
            .map_err(|e| format!("OIDC discovery failed: {e}"))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(client_id.to_string()),
            Some(ClientSecret::new(client_secret.to_string())),
        )
        .set_redirect_uri(redirect);
        return Ok(ProviderClient::Oidc(client));
    }

    let auth_url = provider.auth_url.as_deref()
        .ok_or("auth_url required for non-OIDC provider")?;
    let token_url = provider.token_url.as_deref()
        .ok_or("token_url required for non-OIDC provider")?;
    let userinfo_url = provider.userinfo_url.clone()
        .ok_or("userinfo_url required for non-OIDC provider")?;
    let client = BasicClient::new(
        ClientId::new(client_id.to_string()),
        Some(ClientSecret::new(client_secret.to_string())),
        AuthUrl::new(auth_url.to_string()).map_err(|e| format!("bad auth_url: {e}"))?,
        Some(TokenUrl::new(token_url.to_string()).map_err(|e| format!("bad token_url: {e}"))?),
    )
    .set_redirect_uri(redirect);
    Ok(ProviderClient::Plain { client, userinfo_url })
}

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

#[derive(Deserialize)]
struct PlainUserinfo {
    login: Option<String>,
    name: Option<String>,
    email: Option<String>,
}

pub type SharedAuthState = Arc<AuthState>;

pub async fn providers_list(State(auth): State<SharedAuthState>) -> impl IntoResponse {
    #[derive(Serialize)]
    struct Entry { slug: String, name: String }
    let mut list: Vec<_> = auth.cfg.oidc_providers.iter()
        .filter(|(name, _)| auth.clients.contains_key(*name))
        .map(|(name, p)| Entry { slug: name.clone(), name: p.display(name).to_string() })
        .collect();
    list.sort_by(|a, b| a.slug.cmp(&b.slug));
    axum::Json(list)
}

pub async fn start(
    State(auth): State<SharedAuthState>,
    Path(provider_name): Path<String>,
) -> Response {
    let Some(client) = auth.client(&provider_name) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let (url, csrf_token, pkce_verifier) = match client {
        ProviderClient::Oidc(c) => {
            let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
            let (url, csrf_token, _nonce) = c
                .authorize_url(
                    AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                    CsrfToken::new_random,
                    Nonce::new_random,
                )
                .add_scope(Scope::new("email".into()))
                .add_scope(Scope::new("profile".into()))
                .set_pkce_challenge(pkce_challenge)
                .url();
            (url, csrf_token, pkce_verifier)
        }
        ProviderClient::Plain { client: c, .. } => {
            let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
            let (url, csrf_token) = c
                .authorize_url(CsrfToken::new_random)
                .add_scope(oauth2::Scope::new("read:user".into()))
                .add_scope(oauth2::Scope::new("user:email".into()))
                .set_pkce_challenge(pkce_challenge)
                .url();
            (url, csrf_token, pkce_verifier)
        }
    };

    let state_key = csrf_token.secret().clone();
    let mut pending = auth.pending.lock().await;
    pending.retain(|_, v| v.created_at.elapsed() < PENDING_TTL);
    pending.insert(state_key, OAuthPending {
        pkce_verifier, provider: provider_name, created_at: Instant::now(),
    });
    Redirect::to(url.as_str()).into_response()
}

pub async fn callback(
    State(auth): State<SharedAuthState>,
    Path(provider_name): Path<String>,
    Query(params): Query<CallbackParams>,
    jar: CookieJar,
) -> Response {
    let pending = {
        let mut map = auth.pending.lock().await;
        match map.remove(&params.state) {
            Some(e) if e.created_at.elapsed() < PENDING_TTL && e.provider == provider_name => e,
            _ => return StatusCode::BAD_REQUEST.into_response(),
        }
    };
    let Some(client) = auth.client(&provider_name) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let (email, name) = match client {
        ProviderClient::Oidc(c) => {
            let token = match c
                .exchange_code(OidcCode::new(params.code))
                .set_pkce_verifier(pending.pkce_verifier)
                .request_async(async_http_client)
                .await
            {
                Ok(t) => t,
                Err(e) => { tracing::warn!(err = %e, "token exchange"); return StatusCode::UNAUTHORIZED.into_response(); }
            };
            let userinfo: CoreUserInfoClaims = match c
                .user_info(token.access_token().clone(), None)
                .and_then(|req| Ok(req.request_async(async_http_client)))
            {
                Ok(fut) => match fut.await {
                    Ok(u) => u,
                    Err(e) => { tracing::warn!(err = %e, "userinfo"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
                },
                Err(e) => { tracing::warn!(err = %e, "userinfo req build"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
            };
            let email = match userinfo.email() {
                Some(e) => e.to_string(),
                None => { tracing::warn!("no email from provider"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
            };
            let name = userinfo.name()
                .and_then(|n| n.get(None))
                .map(|n| n.to_string())
                .unwrap_or_else(|| email.clone());
            (email, name)
        }
        ProviderClient::Plain { client: c, userinfo_url } => {
            let token = match c
                .exchange_code(OAuth2Code::new(params.code))
                .set_pkce_verifier(pending.pkce_verifier)
                .request_async(oauth2::reqwest::async_http_client)
                .await
            {
                Ok(t) => t,
                Err(e) => { tracing::warn!(err = %e, "token exchange"); return StatusCode::UNAUTHORIZED.into_response(); }
            };
            let info: PlainUserinfo = match auth.http
                .get(userinfo_url.as_str())
                .bearer_auth(token.access_token().secret())
                .send().await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => { tracing::warn!(err = %e, "userinfo json"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
                },
                Err(e) => { tracing::warn!(err = %e, "userinfo request"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
            };
            let email = match info.email {
                Some(e) => e,
                None => { tracing::warn!("no email from provider"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
            };
            let name = info.name.or(info.login).unwrap_or_else(|| email.clone());
            (email, name)
        }
    };

    let uid = UserStore::email_id(&email);
    let user = User { id: uid, provider: provider_name, subject: email.clone(), email: email.clone(), name };
    if let Err(e) = auth.user_store.upsert(&user).await {
        tracing::error!(err = %e, "upsert user");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let exp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
        + SESSION_MAX_AGE_SECS;
    let claims = SessionClaims { sub: UserStore::email_key(&email), uid, exp };
    let jwt = match encode(
        &Header::default(), &claims,
        &EncodingKey::from_secret(auth.cfg.jwt_secret.as_bytes()),
    ) {
        Ok(t) => t,
        Err(e) => { tracing::error!(err = %e, "jwt encode"); return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
    };
    let cookie = Cookie::build((SESSION_COOKIE, jwt))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::seconds(SESSION_MAX_AGE_SECS))
        .build();
    (jar.add(cookie), Redirect::to("/")).into_response()
}

pub fn validate_session(jar: &CookieJar, secret: &str) -> Option<SessionClaims> {
    let token = jar.get(SESSION_COOKIE)?.value();
    let mut validation = Validation::default();
    validation.validate_exp = true;
    decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()
    .map(|d| d.claims)
}
