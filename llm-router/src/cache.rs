use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry {
    expires: Instant,
    seq: u64,
    value: Value,
}

/// Cache in-memory untuk response chat non-streaming.
/// Key = hash dari (model + body request). Eviksi paling tua (FIFO)
/// kalau jumlah entri melebihi max_entries. Entry kedaluwarsa
/// dibersihkan pelan-pelan setiap kali ada put/get.
pub struct ResponseCache {
    map: Mutex<HashMap<String, Entry>>,
    seq: AtomicU64,
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseCache {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, e| e.expires > now);
        map.get(key).map(|e| e.value.clone())
    }

    pub fn put(&self, key: String, value: Value, ttl: Duration, max_entries: usize) {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, e| e.expires > now);

        if max_entries > 0 && !map.contains_key(&key) && map.len() >= max_entries {
            // Eviksi entri yang paling lama masuk.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        map.insert(
            key,
            Entry {
                expires: now + ttl,
                seq,
                value,
            },
        );
    }

    pub fn clear(&self) -> usize {
        let mut map = self.map.lock().unwrap();
        let n = map.len();
        map.clear();
        n
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
