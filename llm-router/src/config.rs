use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// API key awal yang dipakai client untuk akses router ini.
    /// Di-seed ke database saat startup; selanjutnya bisa dikelola via /admin.
    /// Kalau kosong DAN database tidak punya key, router menerima request tanpa auth.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// API key khusus untuk endpoint /admin/*. Kosong = admin nonaktif.
    #[serde(default)]
    pub admin_key: String,
    /// Path database SQLite untuk usage tracking & api keys.
    #[serde(default = "default_db")]
    pub usage_db: String,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default)]
    pub rate_limit: RateLimit,
    #[serde(default)]
    pub circuit_breaker: CircuitBreaker,
    /// Cache response non-streaming (off by default).
    #[serde(default)]
    pub cache: CacheConfig,
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelRoute>,
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8080
}
fn default_timeout() -> u64 {
    120
}
fn default_db() -> String {
    "usage.db".into()
}
fn default_true() -> bool {
    true
}

/// Rate limit per API key client. 0 = tanpa batas.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RateLimit {
    #[serde(default)]
    pub requests_per_minute: u32,
    #[serde(default)]
    pub requests_per_day: u32,
}

/// Circuit breaker per provider upstream.
#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreaker {
    /// Berapa kegagalan beruntun sebelum provider di-"open".
    #[serde(default = "default_threshold")]
    pub failure_threshold: u32,
    /// Berapa lama provider di-skip sebelum dicoba lagi.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

/// Cache response chat non-streaming.
/// Hanya request yang identik (hash body sama) yang di-cache.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: 300,
            max_entries: 256,
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown_secs: 60,
        }
    }
}

fn default_threshold() -> u32 {
    5
}
fn default_cooldown() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenAI dan semua yang OpenAI-compatible
    /// (OpenRouter, Groq, Together, DeepSeek, Mistral, Ollama, vLLM, dll)
    Openai,
    Anthropic,
    Gemini,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelRoute {
    #[serde(default)]
    pub routes: Vec<RouteDef>,
    /// Harga USD per 1 juta token input (untuk billing). 0 = tidak diatur.
    #[serde(default)]
    pub input_price_per_mtok: f64,
    /// Harga USD per 1 juta token output (untuk billing). 0 = tidak diatur.
    #[serde(default)]
    pub output_price_per_mtok: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteDef {
    pub provider: String,
    pub upstream: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("gagal membaca '{path}': {e}"))?;
        toml::from_str(&raw).map_err(|e| format!("config tidak valid: {e}"))
    }
}
