use crate::cache::ResponseCache;
use crate::config::Config;
use crate::db::{Db, UsageRecord};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

/// Satu jalur routing: provider tujuan + nama model di provider tersebut.
#[derive(Clone, Debug)]
pub struct Route {
    pub provider: String,
    pub upstream: String,
}

/// Sliding window rate limit per key.
#[derive(Default)]
struct RateWindow {
    minute: VecDeque<Instant>,
    day: VecDeque<Instant>,
}

/// State circuit breaker per provider.
#[derive(Default, Clone)]
struct Circuit {
    failures: u32,
    open_until: Option<Instant>,
}

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    pub db: Db,
    /// Cache response non-streaming.
    pub cache: ResponseCache,
    /// Daftar client key aktif (runtime, sinkron dengan DB).
    keys: RwLock<Vec<String>>,
    /// Round-robin counter per provider, untuk distribusi API key upstream.
    counters: Mutex<HashMap<String, usize>>,
    /// Rate limit windows per client key.
    limiter: Mutex<HashMap<String, RateWindow>>,
    /// Circuit breaker per provider upstream.
    circuits: Mutex<HashMap<String, Circuit>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        // Tidak pakai total timeout global supaya streaming tidak terpotong.
        // Timeout per-request hanya diterapkan untuk response non-streaming.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("gagal membuat http client");

        let db_path = if config.usage_db.is_empty() {
            "usage.db".to_string()
        } else {
            config.usage_db.clone()
        };
        let db = match Db::open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("warning: gagal membuka db usage '{db_path}' ({e}), fallback ke in-memory");
                Db::open(":memory:").expect("gagal membuat db in-memory")
            }
        };

        // Seed API key dari config ke DB (idempoten), lalu muat semua key aktif.
        for k in &config.api_keys {
            let _ = db.upsert_key(k, "config");
        }
        let keys: Vec<String> = db
            .keys()
            .map(|ks| ks.into_iter().filter(|k| k.enabled).map(|k| k.key).collect())
            .unwrap_or_default();

        Self {
            config,
            http,
            db,
            cache: ResponseCache::new(),
            keys: RwLock::new(keys),
            counters: Mutex::new(HashMap::new()),
            limiter: Mutex::new(HashMap::new()),
            circuits: Mutex::new(HashMap::new()),
        }
    }

    // ---------- client keys ----------

    /// Kalau tidak ada key terdaftar sama sekali -> mode terbuka (tanpa auth).
    pub fn key_count(&self) -> usize {
        self.keys.read().unwrap().len()
    }

    pub fn valid_key(&self, k: &str) -> bool {
        self.keys.read().unwrap().iter().any(|v| v == k)
    }

    pub fn add_runtime_key(&self, k: String) {
        self.keys.write().unwrap().push(k);
    }

    pub fn revoke_key(&self, k: &str) -> Result<bool, String> {
        let removed_runtime = {
            let mut keys = self.keys.write().unwrap();
            let before = keys.len();
            keys.retain(|x| x != k);
            before != keys.len()
        };
        let removed_db = self.db.revoke_key(k)?;
        Ok(removed_runtime || removed_db)
    }

    // ---------- rate limiting ----------

    /// Sliding window check. true = boleh lewat.
    pub fn check_rate_limit(&self, key: &str) -> bool {
        let rpm = self.config.rate_limit.requests_per_minute;
        let rpd = self.config.rate_limit.requests_per_day;
        if rpm == 0 && rpd == 0 {
            return true;
        }
        let now = Instant::now();
        let mut map = self.limiter.lock().unwrap();
        let w = map.entry(key.to_string()).or_default();

        if rpm > 0 {
            while let Some(t) = w.minute.front() {
                if now.duration_since(*t).as_secs() >= 60 {
                    w.minute.pop_front();
                } else {
                    break;
                }
            }
            if w.minute.len() as u32 >= rpm {
                return false;
            }
        }
        if rpd > 0 {
            while let Some(t) = w.day.front() {
                if now.duration_since(*t).as_secs() >= 86_400 {
                    w.day.pop_front();
                } else {
                    break;
                }
            }
            if w.day.len() as u32 >= rpd {
                return false;
            }
        }
        if rpm > 0 {
            w.minute.push_back(now);
        }
        if rpd > 0 {
            w.day.push_back(now);
        }
        true
    }

    // ---------- upstream provider keys ----------

    /// Ambil API key berikutnya untuk provider (round-robin),
    /// supaya banyak key bisa saling membagi beban / kuota.
    pub fn next_key(&self, provider: &str) -> Option<String> {
        let keys = &self.config.providers.get(provider)?.api_keys;
        if keys.is_empty() {
            return None;
        }
        let mut map = self.counters.lock().unwrap();
        let idx = map.entry(provider.to_string()).or_default();
        let key = keys[*idx % keys.len()].clone();
        *idx = idx.wrapping_add(1);
        Some(key)
    }

    // ---------- model routing ----------

    /// Resolve nama model yang dikirim client jadi daftar kandidat route.
    /// - Alias dari [models] -> semua routes (urutan = urutan fallback)
    /// - "provider/model" -> satu route langsung
    pub fn resolve_model(&self, model: &str) -> Vec<Route> {
        if let Some(m) = self.config.models.get(model) {
            return m
                .routes
                .iter()
                .filter(|r| {
                    self.config
                        .providers
                        .get(&r.provider)
                        .map(|p| p.enabled)
                        .unwrap_or(false)
                })
                .map(|r| Route {
                    provider: r.provider.clone(),
                    upstream: r.upstream.clone(),
                })
                .collect();
        }

        if let Some((provider, upstream)) = model.split_once('/') {
            let enabled = self
                .config
                .providers
                .get(provider)
                .map(|p| p.enabled)
                .unwrap_or(false);
            if enabled {
                return vec![Route {
                    provider: provider.to_string(),
                    upstream: upstream.to_string(),
                }];
            }
        }

        Vec::new()
    }

    // ---------- usage tracking ----------

    pub fn record_usage(&self, r: UsageRecord) {
        if let Err(e) = self.db.insert_usage(&r) {
            tracing::warn!("gagal mencatat usage: {e}");
        }
    }

    // ---------- response cache ----------

    /// Hash deterministik dari (model + body request) untuk cache key.
    pub fn cache_hash(&self, body: &serde_json::Value, model: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(model.as_bytes());
        hasher.update(b"\n");
        hasher.update(body.to_string().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub fn cache_get(&self, key: &str) -> Option<serde_json::Value> {
        if !self.config.cache.enabled {
            return None;
        }
        self.cache.get(key)
    }

    pub fn cache_put(&self, key: &str, value: serde_json::Value) {
        if !self.config.cache.enabled {
            return;
        }
        self.cache.put(
            key.to_string(),
            value,
            Duration::from_secs(self.config.cache.ttl_secs),
            self.config.cache.max_entries,
        );
    }

    pub fn cache_clear(&self) -> usize {
        self.cache.clear()
    }

    // ---------- circuit breaker ----------

    pub fn circuit_open(&self, provider: &str) -> bool {
        let map = self.circuits.lock().unwrap();
        if let Some(c) = map.get(provider) {
            if let Some(until) = c.open_until {
                return Instant::now() < until;
            }
        }
        false
    }

    /// Catat hasil panggilan provider. Kalau gagal beruntun melewati
    /// threshold, provider di-"open" selama cooldown_secs.
    pub fn record_provider_result(&self, provider: &str, ok: bool) {
        let threshold = self.config.circuit_breaker.failure_threshold;
        let cooldown = self.config.circuit_breaker.cooldown_secs;
        let mut map = self.circuits.lock().unwrap();
        let c = map.entry(provider.to_string()).or_default();
        if ok {
            c.failures = 0;
            c.open_until = None;
        } else {
            c.failures += 1;
            if threshold > 0 && c.failures >= threshold {
                c.open_until = Some(Instant::now() + Duration::from_secs(cooldown));
                c.failures = 0;
                tracing::warn!(
                    provider = %provider,
                    cooldown_secs = cooldown,
                    "circuit breaker OPEN, provider di-skip sementara"
                );
            }
        }
    }

    pub fn reset_circuit(&self, provider: &str) {
        self.circuits.lock().unwrap().remove(provider);
    }

    /// Snapshot status circuit semua provider (untuk /admin/providers).
    pub fn circuit_snapshot(&self) -> HashMap<String, (u32, bool)> {
        let map = self.circuits.lock().unwrap();
        self.config
            .providers
            .keys()
            .map(|name| {
                let c = map.get(name);
                let open = c
                    .and_then(|c| c.open_until)
                    .map(|t| Instant::now() < t)
                    .unwrap_or(false);
                let failures = c.map(|c| c.failures).unwrap_or(0);
                (name.clone(), (failures, open))
            })
            .collect()
    }
}
