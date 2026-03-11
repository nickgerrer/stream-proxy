# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build                    # debug build
cargo build --release          # release build
cargo run                      # run locally (listens on :8888)
cargo clippy                   # lint
cargo fmt -- --check           # format check
RUST_LOG=debug cargo run       # run with debug logging
```

No test suite exists yet. There are no workspace members — single crate only.

FFmpeg must be installed and on PATH for transcoding to work (`brew install ffmpeg` on macOS).

## Architecture

Transmitarr-stream-proxy is a Rust MPEG-TS streaming proxy that sits between upstream stream sources and downstream clients. It multiplexes a single upstream connection to many clients via `tokio::sync::broadcast`.

### Module layout

- **`main.rs`** — Axum router setup, health endpoint. All state is `Arc<AppState>`.
- **`state.rs`** — `AppState` (the shared singleton): channel routing table, active channels, account connection limits, stream cooldowns. All fields use `DashMap` or atomics for lock-free concurrent access.
- **`models.rs`** — Serde request/response types for the Control and Status APIs.
- **`control.rs`** — Control API handlers (PUT/DELETE channels, PUT accounts, POST sync). The Laravel backend pushes routing config here.
- **`stream.rs`** — Client-facing `GET /stream/{channel_id}` handler. Subscribes clients to the broadcast channel, sends TS null packets as keepalives. Uses a `ClientGuard` drop guard for cleanup on disconnect. Supports optional FFmpeg transcoding via `?transcode=1`.
- **`transcode.rs`** — FFmpeg subprocess management. `spawn_ffmpeg` spawns a child process with `kill_on_drop(true)`, pipes broadcast data to stdin, reads transcoded output from stdout.
- **`upstream.rs`** — Upstream connection lifecycle. `start_channel` spawns a tokio task that fetches from the upstream URL, buffers to 188KB-aligned chunks, and broadcasts. Handles failover (up to 10 attempts) across streams/accounts.
- **`status.rs`** — Status API: list channels, channel detail with per-client stats.

### Key data flow

1. Laravel pushes channel routing via `/control/v1/sync` or individual PUT endpoints
2. Client connects to `/stream/{channel_id}` → if no active upstream, `select_stream` picks a stream+account respecting cooldowns and connection limits → `start_channel` spawns upstream reader
3. Upstream reader buffers bytes into 188KB chunks (TS-packet-aligned) and broadcasts via `broadcast::channel`
4. Each client receives chunks from its `broadcast::Receiver`, with 500ms TS null packet keepalives
5. When last client disconnects (detected via `ClientGuard` drop), upstream is stopped

### Concurrency model

All shared state uses `DashMap` (concurrent hashmap) and `AtomicU32`/`AtomicU64` — no `Mutex`/`RwLock`. Account connection limits use `fetch_update` to prevent underflow races. A background task runs every 5 minutes to clean up expired cooldowns.

### FFmpeg transcoding

Clients requesting `?transcode=1` get their stream piped through a per-client FFmpeg subprocess:
- Video passthrough with SPS/PPS injection (`-c:v copy -bsf:v dump_extra`)
- Audio transcoded to AC3 128kbps (`-c:a ac3 -b:a 128k`)
- PTS generation and corrupt packet discard (`-fflags +genpts+discardcorrupt`)

The FFmpeg child process is killed automatically on client disconnect via `kill_on_drop(true)`. If FFmpeg crashes, it restarts up to 3 times, then falls back to raw passthrough. The `transcode` module (`src/transcode.rs`) handles subprocess spawning; `stream.rs` branches between raw and transcoded response bodies.

### Stream cooldowns

Failed upstream connections are recorded with a 30-minute cooldown (`cooldown_duration()` in `state.rs`). `select_stream` skips cooled-down stream+account pairs, but if ALL streams are cooled down it ignores cooldowns as a fallback. Cooldowns are exposed in the channel detail response and can be cleared via the control API.

### API routes

- `PUT /control/v1/channels/{channel_id}` — set channel routing config
- `DELETE /control/v1/channels/{channel_id}` — remove channel + stop stream
- `PUT /control/v1/accounts/{account_id}` — set account connection limit
- `POST /control/v1/sync` — full state sync (replaces all channels/accounts, clears cooldowns)
- `DELETE /control/v1/channels/{channel_id}/cooldowns` — clear cooldowns for one channel
- `DELETE /control/v1/cooldowns` — clear all cooldowns
- `GET /stream/{channel_id}` — MPEG-TS stream (video/mp2t). Add `?transcode=1` for FFmpeg transcoding (Roku/Plex compatibility)
- `GET /status/v1/channels` — all channel + account status
- `GET /status/v1/channels/{channel_id}` — single channel detail with clients
- `GET /status/v1/health` — health check

## CI

GitHub Actions builds and pushes a multi-arch Docker image (amd64+arm64) to GHCR on push to main. Uses rustls (no OpenSSL dependency).
