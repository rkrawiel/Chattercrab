# Deploying Chattercrab

This guide runs the Chattercrab signaling server on a Linux VPS with Docker Compose.
Traefik terminates TLS and reverse-proxies to `chattercrab-signal`, which speaks plain
HTTP on the Compose network. There is no Coturn and no TURN relay. Media stays on the
direct WebRTC path. Peers that cannot connect directly get no relay fallback.

Local development stays `cargo run` on plain HTTP with no password. That is not the VPS path.

## 1. DNS

Create an **A** record for the name clients will use (for example `chat.example.com`)
pointing at the VPS public IPv4 address. Add an **AAAA** record as well when the VPS
has a public IPv6 address. Traefik requests the certificate for `CHATTERCRAB_DOMAIN`,
so the name must resolve to this machine before you start the stack.

## 2. Docker

Install Docker Engine and the Docker Compose plugin from Docker's Debian or Ubuntu
packages. Do not install Coturn.

The VPS needs outbound HTTPS so Traefik can reach Let's Encrypt and so image pulls
succeed. The signaling container does not need a public IP of its own.

## 3. Firewall

Allow only:

| Port | Protocol | Purpose |
|------|----------|---------|
| 22 | tcp | SSH |
| 80 | tcp | HTTP to HTTPS redirect, and the Let's Encrypt HTTP-01 challenge |
| 443 | tcp | Traefik, HTTPS and WSS for clients |

Do not open 3478, 5349, a TURN relay range, or 8443. The Rust process is not published
on the host. It listens on port 8080 inside the Compose network, and only Traefik
connects to it.

Example with ufw, after confirming the SSH rule is in place:

```sh
ufw allow 22/tcp
ufw allow 80/tcp
ufw allow 443/tcp
ufw enable
```

Cloud security groups need the same three ports.

## 4. Configure

On the VPS, clone this repository and create the env file from the template:

```sh
cp .env.example .env
```

Set:

| Variable | Meaning |
|----------|---------|
| `CHATTERCRAB_DOMAIN` | DNS name from section 1. Required. |
| `ACME_EMAIL` | Let's Encrypt account email. Required. |
| `CHATTERCRAB_PASSWORD` | Shared signaling password. Required. Compose refuses to start when it is missing. |
| `CHATTERCRAB_AUTH_USER` | Basic auth user. Defaults to `chattercrab` when unset. |
| `CHATTERCRAB_STUN_URL` | One `stun:` URL. Defaults to `stun:stun.l.google.com:19302` when unset. |

`.env` is gitignored. Do not commit a real password.

A dollar sign inside the password must be written as two dollar signs in the env file
because Compose interpolates `$`. A password of `pa$$word` is written `pa$$$$word`.

Optional overrides, with the defaults Compose uses when they are absent:

| Variable | Default |
|----------|---------|
| `CHATTERCRAB_MAX_ROOM_SIZE` | `5` |
| `CHATTERCRAB_MAX_ROOMS` | `100` |
| `CHATTERCRAB_ROOM_IDLE_TTL_SECS` | `3600` |
| `RUST_LOG` | `info,chattercrab_server=info` |

The container sets `CHATTERCRAB_BIND=0.0.0.0:8080` and does not set
`CHATTERCRAB_TLS_CERT` or `CHATTERCRAB_TLS_KEY`.

## 5. Start

From the repository root:

```sh
docker compose up -d --build
```

That builds `chattercrab-signal:local`, starts Traefik on host ports 80 and 443, and
starts the server with no published port. Traefik obtains a certificate with the
HTTP-01 challenge and stores it in the `traefik-certs` volume.

The first certificate request needs public DNS and port 80 reachable from the
internet. Until it succeeds, HTTPS to the name fails.

## 6. Clients

In the desktop client, set the server URL to `https://chat.example.com` (your domain,
port 443, no `:8443`). Set the same user and password as `CHATTERCRAB_AUTH_USER` and
`CHATTERCRAB_PASSWORD`.

Check from another machine:

```sh
curl -fsS https://chat.example.com/health
curl -sS -o /dev/null -w '%{http_code}\n' https://chat.example.com/rooms
```

`/health` succeeds with no password. `/rooms` with no password returns `401`.

## 7. Logs

```sh
docker compose logs -f chattercrab
docker compose logs -f traefik
```

## 8. Upgrade

```sh
git pull
docker compose up -d --build
```

Rooms are in memory and are lost on recreate.

## 9. STUN

STUN is external. `CHATTERCRAB_STUN_URL` is handed to clients by `GET /ice-servers`.
The VPS does not relay media. Peers that cannot connect directly get no TURN fallback.

## 10. Troubleshooting

| Symptom | What to check |
|---------|----------------|
| Certificate not issued | DNS A or AAAA must point at this VPS, and TCP port 80 must reach Traefik. `docker compose logs -f traefik` shows the HTTP-01 attempt. |
| Client or `curl` gets `401` | The password (and user) must match `CHATTERCRAB_PASSWORD` and `CHATTERCRAB_AUTH_USER`. Remember `$$` for a literal `$` in `.env`. `/health` stays open, so a `401` there means the request did not hit this server. |
| ICE failure, peers never connect | A client firewall is blocking UDP, or the configured STUN URL is unreachable from that client. There is no TURN fallback on the VPS. |

## 11. Stop

```sh
docker compose down
```

`docker compose down -v` also deletes the `traefik-certs` volume, so the next start
requests a new certificate.

## 12. Building on Linux (client prerequisites)

The desktop client links system libraries for audio, windowing and screen capture.
The Docker image does not build the client. This list is for `cargo` on a Linux
workstation or for `deploy/build-release.sh`.

Debian/Ubuntu packages:

```sh
sudo apt-get install -y build-essential cmake pkg-config clang libclang-dev nasm \
    libasound2-dev \
    libx11-dev libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev libxcb-xfixes0-dev libxrandr-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
    libpipewire-0.3-dev libdbus-1-dev libegl1-mesa-dev libgbm-dev \
    musl-tools mingw-w64
```

| Package group | Needed by |
|---------------|-----------|
| `cmake`, `build-essential` | bundled libopus (`opusic-sys`), OpenH264 (C++) |
| `libasound2-dev` | `cpal` (ALSA backend; PulseAudio and PipeWire are reached through their ALSA plugins) |
| `libx11`, `libxcb*`, `libxrandr` | `xcap` X11 screen capture, `winit` |
| `libxkbcommon*`, `libwayland-dev`, `libegl1-mesa-dev` | `winit`/`iced` windowing and rendering |
| `libpipewire-0.3-dev`, `clang`, `libclang-dev`, `libgbm-dev` | `xcap` Wayland/PipeWire support (bindgen); capture on Wayland is a documented follow-up |
| `libdbus-1-dev` | `audio_thread_priority` (rtkit) if built with dbus |
| `musl-tools`, `mingw-w64` | plain-cargo cross builds when zig is not available |

Runtime on the desktop: an X11 session (Xorg or XWayland for the window; screen capture
requires X11), and a working ALSA/PulseAudio/PipeWire audio stack.

### Verifying in WSL (development machines running Windows)

`deploy/wsl-verify.sh` runs fmt, clippy, tests, a release build, `deploy/build-release.sh`,
shellcheck of the remaining deploy shell scripts, and an idle RSS measurement of the
musl server. From Git Bash:

```sh
wsl --install -d Ubuntu-24.04 --no-launch
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-24.04 -u root --exec sh -c \
    'printf "[boot]\nsystemd=true\n" > /etc/wsl.conf'
wsl --terminate Ubuntu-24.04
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-24.04 -u root --exec bash \
    /mnt/d/Chattercrab/deploy/wsl-verify.sh --setup /mnt/d/Chattercrab
```

The script builds in `~/chattercrab-verify` inside WSL because builds on `/mnt/d` are slow.
