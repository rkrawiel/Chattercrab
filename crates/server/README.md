# chattercrab-server

Signaling server for Chattercrab, built as the `chattercrab-signal` binary. It hosts rooms,
relays SDP offers/answers and ICE candidates between members, keeps the last 200 chat
messages per room in memory, and arbitrates the single screen-share slot per room. It
never relays media. Clients gather server-reflexive candidates from one external STUN URL
(`GET /ice-servers`). There is no TURN relay. The wire format is specified in
[`docs/PROTOCOL.md`](../../docs/PROTOCOL.md).

## Configuration

All settings come from environment variables. The binary defaults are below. The VPS
container overrides bind, and Compose requires a password (see [Run on a VPS](#run-on-a-vps)).

| Variable | Default | Meaning |
|----------|---------|---------|
| `CHATTERCRAB_BIND` | `0.0.0.0:8443` | Listen address. The container sets `0.0.0.0:8080`. |
| `CHATTERCRAB_TLS_CERT`, `CHATTERCRAB_TLS_KEY` | unset | PEM certificate chain and key. Both or neither. Unset, the server speaks plain HTTP and WS. The container leaves them unset; Traefik terminates TLS. |
| `CHATTERCRAB_STUN_URL` | `stun:stun.l.google.com:19302` | Exactly one `stun:host:port` (or `stun:[ipv6]:port`) URL returned by `GET /ice-servers`. `turn:` and `turns:` are rejected. |
| `CHATTERCRAB_AUTH_USER` | `chattercrab` | Basic auth user, used only when `CHATTERCRAB_PASSWORD` is set. Must be non-empty and must not contain `:`. |
| `CHATTERCRAB_PASSWORD` | unset | Shared password. Empty or unset disables authentication. When set, every route except `GET /health` and CORS preflight requires `Authorization: Basic`. Compose requires a non-empty value. |
| `CHATTERCRAB_MAX_ROOM_SIZE` | `5` | Members per room. |
| `CHATTERCRAB_MAX_ROOMS` | `100` | Rooms per server. |
| `CHATTERCRAB_ROOM_IDLE_TTL_SECS` | `3600` | Empty rooms idle this long are evicted (checked every 60 s). |
| `RUST_LOG` | `info` | `tracing` filter. Logs are JSON lines on stdout. The container default is `info,chattercrab_server=info`. |

Fixed limits: join timeout 5 s, WebSocket Ping every 20 s (closed after 2 missed Pongs),
outbound queue 256 messages per connection, WebSocket messages up to 64 KiB, REST bodies
up to 16 KiB, `POST /rooms` 10 per minute per IP (burst 5), `GET /ice-servers` 30 per
minute per IP.

Signals: `SIGHUP` reloads the certificate without dropping connections (a failed reload
keeps the old one); `SIGTERM`/`SIGINT` stop accepting, send close `1001` to every socket
and exit within 10 s.

## Run on a VPS

The supported VPS path is Docker Compose with Traefik. From the repository root, copy
`.env.example` to `.env`, set the domain, ACME email, and password, then run
`docker compose up -d --build`. Details are in [`docs/DEPLOY.md`](../../docs/DEPLOY.md).

The container uses these variables:

| Variable | Container value |
|----------|-----------------|
| `CHATTERCRAB_BIND` | `0.0.0.0:8080` (plain HTTP, no TLS certificate) |
| `CHATTERCRAB_STUN_URL` | `stun:stun.l.google.com:19302` unless overridden |
| `CHATTERCRAB_AUTH_USER` | `chattercrab` unless overridden |
| `CHATTERCRAB_PASSWORD` | required by Compose |
| `CHATTERCRAB_MAX_ROOM_SIZE` | `5` unless overridden |
| `CHATTERCRAB_MAX_ROOMS` | `100` unless overridden |
| `CHATTERCRAB_ROOM_IDLE_TTL_SECS` | `3600` unless overridden |
| `RUST_LOG` | `info,chattercrab_server=info` unless overridden |

TLS is not configured inside the container. Traefik publishes ports 80 and 443 and
proxies to port 8080 on the Compose network. Clients use `https://<CHATTERCRAB_DOMAIN>`.

## Run locally

```sh
CHATTERCRAB_BIND=127.0.0.1:8443 RUST_LOG=debug cargo run -p chattercrab-server
curl http://127.0.0.1:8443/health
curl -X POST -H 'content-type: application/json' -d '{"name":"test"}' http://127.0.0.1:8443/rooms
```

## Test

```sh
cargo test -p chattercrab-server
```

Unit tests cover configuration parsing, including the STUN URL and optional Basic auth (the password is redacted in `Debug`). Integration tests in `tests/` start the server in-process on `127.0.0.1:0` with
shortened timers and drive it over HTTP and WebSocket: rooms, limits, join/welcome,
relay, heartbeat, join timeout, room full, screen lock, chat history, slow-consumer kick,
room deletion, graceful shutdown, and HTTPS with certificate hot reload.

## Static Linux build

```sh
cargo zigbuild --release -p chattercrab-server --target x86_64-unknown-linux-musl
```

Produces a fully static ELF (no `PT_INTERP`, no `DT_NEEDED`), about 3.9 MB.
