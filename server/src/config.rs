use figment::{Figment, providers::{Env, Format, Serialized, Toml}};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single OIDC/OAuth2 provider. Routes: `/auth/{name}` and `/auth/{name}/callback`.
///
/// Two modes:
/// - **OIDC** (`issuer_url` set): endpoints are discovered from the issuer's
///   `/.well-known/openid-configuration`. Works out of the box for Google, GitLab,
///   Auth0, Keycloak, etc.
/// - **Plain OAuth2** (no `issuer_url`): `auth_url`, `token_url`, and `userinfo_url`
///   must all be provided. Use for providers that do not implement OIDC (e.g. GitHub).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcProvider {
    /// Supplied via env: `NETGOL_OIDC_PROVIDERS__<NAME>__CLIENT_ID`
    pub client_id: Option<String>,
    /// Supplied via env: `NETGOL_OIDC_PROVIDERS__<NAME>__CLIENT_SECRET`
    pub client_secret: Option<String>,
    /// OIDC issuer URL. When set, all endpoints are discovered automatically.
    pub issuer_url: Option<String>,
    /// OAuth2-only: authorization endpoint URL (required when `issuer_url` is absent).
    pub auth_url: Option<String>,
    /// OAuth2-only: token endpoint URL (required when `issuer_url` is absent).
    pub token_url: Option<String>,
    /// OAuth2-only: userinfo endpoint URL (required when `issuer_url` is absent).
    pub userinfo_url: Option<String>,
    /// Human-readable label shown in the login modal.
    pub display_name: Option<String>,
}

impl OidcProvider {
    pub fn display<'a>(&'a self, name: &'a str) -> &'a str {
        self.display_name.as_deref().unwrap_or(name)
    }

    /// Redirect URI derived from `base_url` and the provider's map key.
    pub fn redirect_uri(&self, base_url: &str, name: &str) -> String {
        format!("{}/auth/{}/callback", base_url.trim_end_matches('/'), name)
    }
}

/// Central server configuration.
/// Loading order: built-in defaults < TOML file < `NETGOL_` env vars.
/// Every field is individually overridable via `NETGOL_<FIELD_NAME>` in upper-snake-case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub bind: String,
    pub metrics_bind: String,
    pub tick_hz: u32,
    pub max_live_chunks: usize,
    pub peer_outbound_capacity: usize,
    pub snapshot_path: PathBuf,
    pub snapshot_interval_ticks: u64,
    pub regions_path: PathBuf,

    pub client_max_chunks: u32,

    pub oscillator_detection_interval_ms: u64,
    pub oscillator_detection_max_chunks_per_step: usize,
    pub oscillator_promote_max_per_tick: usize,
    pub oscillator_max_group_size: usize,

    /// Publicly reachable base URL (no trailing slash).
    /// Used to construct OIDC redirect URIs and any other absolute URLs.
    pub base_url: String,

    /// Keyed by provider slug (e.g. `"google"`).
    /// TOML: `[oidc_providers.google]`
    /// Env:  `NETGOL_OIDC_PROVIDERS__GOOGLE__CLIENT_ID`
    pub oidc_providers: HashMap<String, OidcProvider>,
    pub jwt_secret: String,

    pub claim_w_chunks: u32,
    pub claim_h_chunks: u32,
    pub users_dir: PathBuf,
    pub claims_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            metrics_bind: "0.0.0.0:9090".into(),
            tick_hz: 10,
            max_live_chunks: 100_000,
            peer_outbound_capacity: 4096,
            snapshot_path: PathBuf::from("data/world.snap"),
            snapshot_interval_ticks: 600,
            regions_path: PathBuf::from("config/regions.toml"),
            client_max_chunks: 65535,
            oscillator_detection_interval_ms: 250,
            oscillator_detection_max_chunks_per_step: 1000,
            oscillator_promote_max_per_tick: 256,
            oscillator_max_group_size: 16,
            base_url: "http://localhost:8080".into(),
            oidc_providers: HashMap::new(),
            jwt_secret: "change-me".into(),
            claim_w_chunks: 3,
            claim_h_chunks: 2,
            users_dir: PathBuf::from("data/users"),
            claims_dir: PathBuf::from("data/claims"),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        let cfg: Self = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("NETGOL_").split("__"))
            .extract()
            .unwrap_or_else(|e| panic!("config error: {e}"));
        cfg.validate();
        cfg
    }

    fn validate(&self) {
        assert!(self.tick_hz > 0, "config: tick_hz must be > 0");
        assert!(self.snapshot_interval_ticks > 0, "config: snapshot_interval_ticks must be > 0");
        assert!(self.peer_outbound_capacity > 0, "config: peer_outbound_capacity must be > 0");
        assert!(self.max_live_chunks > 0, "config: max_live_chunks must be > 0");
        assert!(self.oscillator_max_group_size >= 2, "config: oscillator_max_group_size must be >= 2");
        assert!(self.claim_w_chunks >= 1, "config: claim_w_chunks must be >= 1");
        assert!(self.claim_h_chunks >= 1, "config: claim_h_chunks must be >= 1");
    }
}
