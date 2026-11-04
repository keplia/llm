# LLM Router

OpenAI-compatible LLM router/gateway (mirip 9router / LiteLLM / one-api) yang
ditulis dalam Rust. Client cukup ngomong pakai format OpenAI, router yang
meneruskan ke provider manapun.

## Fitur

- ✅ **OpenAI-compatible API** — `/v1/chat/completions`, `/v1/models`, `/v1/embeddings`
- ✅ **Multi-provider** — OpenAI, Anthropic (Claude), Google Gemini, plus semua
  yang OpenAI-compatible (OpenRouter, Groq, Together, DeepSeek, Mistral, Ollama, vLLM)
- ✅ **Konversi format otomatis** — request Anthropic/Gemini ditransform dari
  format OpenAI, response-nya dibalikin ke format OpenAI
- ✅ **Streaming SSE** — response streaming dikonversi real-time dari
  Anthropic/Gemini ke chunk format OpenAI
- ✅ **Tool calls / function calling** — didukung lintas provider (termasuk streaming)
- ✅ **Gambar (vision)** — image URL & base64 data URI
- ✅ **Failover** — kalau provider primary gagal (5xx/429/timeout), otomatis
  pindah ke provider fallback berikutnya
- ✅ **Circuit breaker** — provider yang gagal beruntun di-skip sementara,
  auto-recover setelah cooldown (bisa di-reset manual via admin API)
- ✅ **Load balancing API key** — banyak API key per provider, dipakai round-robin
- ✅ **Auth untuk client** — router dilindungi API key (constant-time compare)
- ✅ **Rate limiting per key** — batas request/menit & request/hari (sliding window)
- ✅ **Usage tracking (SQLite)** — setiap request tercatat: key, model, provider,
  token in/out, latency, status, streaming atau bukan
- ✅ **Billing per token** — set harga (USD/1M token) per model di config,
  biaya dihitung otomatis & tampil di statistik dan dashboard
- ✅ **Response caching** — cache request non-streaming yang identik (hash body),
  TTL + batas entri, off by default
- ✅ **Admin API** — kelola client key (tambah/revoke tanpa restart), lihat
  statistik usage, pengeluaran per key, status provider, reset circuit breaker
- ✅ **Web dashboard** — halaman admin di `http://host:8080/admin`
  (statistik, usage, keys, providers, cache — tanpa dependency tambahan)
- ✅ **Alias model** — client panggil `claude-sonnet`, router yang terjemahkan
  ke `anthropic/claude-sonnet-4-5`, dll. Bisa juga langsung `provider/model`
- ✅ **CORS + request logging** (tracing)
- ✅ **Satu binary kecil** — gampang deploy di VPS Ubuntu

## 1. Install Rust di Ubuntu

```bash
sudo apt update && sudo apt install -y curl build-essential pkg-config git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version
```

## 2. Build

```bash
cd llm-router
cargo build --release
# binary ada di target/release/llm-router
```

Build pertama butuh beberapa menit (download dependencies + compile).

## 3. Konfigurasi

```bash
cp config.example.toml config.toml
nano config.toml   # isi API key provider yang kamu punya
```

Konsep routing:

```toml
[models."claude-sonnet"]
routes = [
  { provider = "anthropic",  upstream = "claude-sonnet-4-5" },   # primary
  { provider = "openrouter", upstream = "anthropic/claude-sonnet-4.5" },  # fallback
]
```

Client minta model `claude-sonnet` → dicoba ke Anthropic dulu, kalau gagal
(5xx/429/timeout) otomatis pindah ke OpenRouter.

Tanpa alias juga bisa: kirim `"model": "openrouter/meta-llama/llama-3.1-70b-instruct"`.

## 4. Jalankan

```bash
CONFIG_PATH=config.toml ./target/release/llm-router
# LLM router listening on http://0.0.0.0:8080
```

## 5. Test

```bash
# Cek hidup
curl http://localhost:8080/health

# Daftar model
curl http://localhost:8080/v1/models \
  -H "Authorization: Bearer sk-router-local-123"

# Chat (non-streaming)
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-router-local-123" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "halo, perkenalkan diri"}]
  }'

# Chat (streaming)
curl -N http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-router-local-123" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet",
    "stream": true,
    "messages": [{"role": "user", "content": "ceritakan dong 1 paragraf"}]
  }'
```

Pakai dari Python (library `openai`):

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://IP-SERVER:8080/v1",
    api_key="sk-router-local-123",
)

resp = client.chat.completions.create(
    model="claude-sonnet",
    messages=[{"role": "user", "content": "halo!"}],
)
print(resp.choices[0].message.content)
```

## 6. Deploy sebagai service (systemd)

```bash
sudo nano /etc/systemd/system/llm-router.service
```

```ini
[Unit]
Description=LLM Router
After=network.target

[Service]
User=www-data
WorkingDirectory=/opt/llm-router
ExecStart=/opt/llm-router/llm-router
Environment=CONFIG_PATH=/opt/llm-router/config.toml
Environment=RUST_LOG=info
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

```bash
sudo mkdir -p /opt/llm-router
sudo cp target/release/llm-router config.toml /opt/llm-router/
sudo systemctl daemon-reload
sudo systemctl enable --now llm-router
sudo systemctl status llm-router
journalctl -u llm-router -f   # lihat log
```

## 7. Docker (alternatif)

```bash
docker build -t llm-router .
docker run -d --name llm-router -p 8080:8080 \
  -v $(pwd)/config.toml:/app/config.toml llm-router
```

## Struktur kode

```
src/
├── main.rs               # entry point: config, tracing, server axum
├── config.rs             # struct config (TOML)
├── state.rs              # AppState: http client, round-robin key, resolver,
│                         #   rate limiter, circuit breaker, runtime keys
├── db.rs                 # SQLite: usage tracking + daftar client key
├── error.rs              # error format OpenAI-compatible
├── auth.rs               # middleware Bearer auth + rate limit + identitas key
├── admin.rs              # endpoint /admin/* (keys, stats, providers)
├── handlers.rs           # endpoint /v1/* + logika failover
└── providers/
    ├── mod.rs            # dispatcher per jenis provider
    ├── openai.rs         # pass-through (OpenAI/OpenRouter/Groq/Ollama/dll)
    ├── anthropic.rs      # transform request/response + streaming Anthropic
    ├── gemini.rs         # transform request/response + streaming Gemini
    └── util.rs           # helper umum + usage logging
```

## Admin API & Web Dashboard

Buka **`http://host:8080/admin`** di browser untuk dashboard (masukkan admin key,
tersimpan di localStorage). Dashboard punya 4 tab: Overview (statistik + biaya),
Usage terakhir, API Keys (generate/revoke), Providers (circuit breaker + cache).

Semua endpoint admin butuh header `Authorization: Bearer <admin_key>`.
Kalau `admin_key` kosong di config, admin API otomatis nonaktif (403).

```bash
ADMIN="Authorization: Bearer admin-secret-123"

# Statistik usage 7 hari terakhir (agregat per hari x model x provider)
curl http://localhost:8080/admin/stats?days=7 -H "$ADMIN"

# Total pemakaian & biaya per API key
curl http://localhost:8080/admin/spend?days=30 -H "$ADMIN"

# 100 request terakhir (per key, token, latency, status, biaya)
curl http://localhost:8080/admin/usage?limit=100 -H "$ADMIN"

# Daftar semua client key
curl http://localhost:8080/admin/keys -H "$ADMIN"

# Buat client key baru (tanpa restart!)
curl -X POST http://localhost:8080/admin/keys -H "$ADMIN" \
  -H "Content-Type: application/json" -d '{"name": "budi"}'
# -> {"key":"sk-3f9c...","name":"budi"} — langsung bisa dipakai client

# Revoke key
curl -X DELETE http://localhost:8080/admin/keys/sk-3f9c... -H "$ADMIN"

# Status provider + circuit breaker
curl http://localhost:8080/admin/providers -H "$ADMIN"

# Paksa tutup circuit breaker provider tertentu
curl -X POST http://localhost:8080/admin/providers/anthropic/reset -H "$ADMIN"

# Status / kosongkan response cache
curl http://localhost:8080/admin/cache -H "$ADMIN"
curl -X DELETE http://localhost:8080/admin/cache -H "$ADMIN"
```

Rate limit, circuit breaker, cache, dan harga token dikendalikan dari `config.toml`:

```toml
[rate_limit]
requests_per_minute = 60   # 0 = unlimited
requests_per_day = 0       # 0 = unlimited

[circuit_breaker]
failure_threshold = 5      # gagal beruntun x kali -> provider di-skip
cooldown_secs = 60         # ... selama 60 detik, lalu dicoba lagi

[cache]
enabled = false            # cache response non-streaming yang identik
ttl_secs = 300
max_entries = 256

[models."gpt-4o"]
input_price_per_mtok = 2.5    # USD per 1 juta token input
output_price_per_mtok = 10.0  # USD per 1 juta token output
```

## Roadmap / bisa ditambah nanti

- Quota/budget bulanan per API key (block otomatis kalau lewat)
- Auto-refresh harga dari API provider
- Multi-user / role untuk admin
