use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::sync::Mutex;

/// Satu entri log pemakaian.
pub struct UsageRecord {
    pub api_key: Option<String>,
    pub model: String,
    pub provider: String,
    pub upstream: String,
    pub status: String, // "ok" | "error"
    pub duration_ms: u64,
    pub stream: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Biaya USD yang dihitung dari harga per model.
    pub cost_usd: f64,
}

pub struct KeyRow {
    pub key: String,
    pub name: String,
    pub created_at: i64,
    pub enabled: bool,
}

/// Wrapper SQLite yang aman dipakai dari banyak task
/// (Connection: Send, dibungkus Mutex biar Sync).
pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    api_key TEXT,
    model TEXT NOT NULL,
    provider TEXT NOT NULL,
    upstream_model TEXT NOT NULL,
    status TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    stream INTEGER NOT NULL,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage(ts);
CREATE INDEX IF NOT EXISTS idx_usage_key ON usage(api_key);
CREATE TABLE IF NOT EXISTS api_keys (
    key TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1
);
";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Db {
    pub fn open(path: &str) -> Result<Self, String> {
        let conn =
            Connection::open(path).map_err(|e| format!("gagal buka sqlite '{path}': {e}"))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("gagal inisialisasi schema: {e}"))?;
        // Migrasi untuk database lama yang belum punya kolom cost_usd.
        let _ = conn.execute_batch("ALTER TABLE usage ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0;");
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---------- usage ----------

    pub fn insert_usage(&self, r: &UsageRecord) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO usage (ts, api_key, model, provider, upstream_model, status, duration_ms, stream, prompt_tokens, completion_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                now(),
                r.api_key,
                r.model,
                r.provider,
                r.upstream,
                r.status,
                r.duration_ms as i64,
                r.stream as i64,
                r.prompt_tokens as i64,
                r.completion_tokens as i64,
                r.cost_usd
            ],
        )
        .map_err(|e| format!("gagal insert usage: {e}"))?;
        Ok(())
    }

    /// Statistik agregat per hari x model x provider.
    pub fn stats(&self, days: u32) -> Result<Value, String> {
        let since = now() - (days as i64) * 86_400;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT date(ts, 'unixepoch') AS day, model, provider,
                        COUNT(*) AS requests,
                        COALESCE(SUM(prompt_tokens), 0),
                        COALESCE(SUM(completion_tokens), 0),
                        COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(cost_usd), 0)
                 FROM usage
                 WHERE ts >= ?1
                 GROUP BY day, model, provider
                 ORDER BY day DESC, requests DESC",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![since], |row| {
                Ok(json!({
                    "day": row.get::<_, String>(0)?,
                    "model": row.get::<_, String>(1)?,
                    "provider": row.get::<_, String>(2)?,
                    "requests": row.get::<_, i64>(3)?,
                    "prompt_tokens": row.get::<_, i64>(4)?,
                    "completion_tokens": row.get::<_, i64>(5)?,
                    "success": row.get::<_, i64>(6)?,
                    "cost_usd": row.get::<_, f64>(7)?,
                }))
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(json!({"days": days, "rows": out}))
    }

    /// N request terakhir.
    pub fn recent(&self, limit: i64) -> Result<Value, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT ts, api_key, model, provider, upstream_model, status,
                        duration_ms, stream, prompt_tokens, completion_tokens, cost_usd
                 FROM usage ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(json!({
                    "ts": row.get::<_, i64>(0)?,
                    "api_key": row.get::<_, Option<String>>(1)?,
                    "model": row.get::<_, String>(2)?,
                    "provider": row.get::<_, String>(3)?,
                    "upstream_model": row.get::<_, String>(4)?,
                    "status": row.get::<_, String>(5)?,
                    "duration_ms": row.get::<_, i64>(6)?,
                    "stream": row.get::<_, i64>(7)? != 0,
                    "prompt_tokens": row.get::<_, i64>(8)?,
                    "completion_tokens": row.get::<_, i64>(9)?,
                    "cost_usd": row.get::<_, f64>(10)?,
                }))
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(json!({"rows": out}))
    }

    /// Total pemakaian & biaya per API key.
    pub fn spend(&self, days: u32) -> Result<Value, String> {
        let since = now() - (days as i64) * 86_400;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(api_key, '(anonymous)') AS api_key,
                        COUNT(*) AS requests,
                        COALESCE(SUM(prompt_tokens), 0),
                        COALESCE(SUM(completion_tokens), 0),
                        COALESCE(SUM(cost_usd), 0)
                 FROM usage
                 WHERE ts >= ?1
                 GROUP BY api_key
                 ORDER BY SUM(cost_usd) DESC",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![since], |row| {
                Ok(json!({
                    "api_key": row.get::<_, String>(0)?,
                    "requests": row.get::<_, i64>(1)?,
                    "prompt_tokens": row.get::<_, i64>(2)?,
                    "completion_tokens": row.get::<_, i64>(3)?,
                    "cost_usd": row.get::<_, f64>(4)?,
                }))
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(json!({"days": days, "rows": out}))
    }

    // ---------- api keys ----------

    pub fn upsert_key(&self, key: &str, name: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO api_keys (key, name, created_at, enabled) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(key) DO NOTHING",
            params![key, name, now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT key, name, created_at, enabled FROM api_keys ORDER BY created_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(KeyRow {
                    key: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    enabled: row.get(3)? != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn revoke_key(&self, key: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("DELETE FROM api_keys WHERE key = ?1", params![key])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }
}
