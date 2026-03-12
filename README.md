# OpenRok

OpenRok is a Rust-based tunneling tool for exposing a local HTTP service through a relay server. The current implementation includes a relay, a CLI client, shared protocol types, host-based routing, binary-safe body forwarding, subdomain reservation, and basic rate limiting.

## Workspace

- `apps/server`: relay server
- `apps/client`: CLI client
- `apps/shared`: shared control protocol
- `PRD.md`: product requirements and planned direction
- `AGENTS.md`: contributor guide

## Current Flow

1. The client requests a tunnel from the relay.
2. The relay reserves a subdomain and returns a registration token.
3. The client opens a persistent websocket control channel.
4. Public HTTP requests are routed by `Host` and forwarded to `localhost:<port>`.

## Requirements

- Docker
- A local app listening on a port such as `3000`

Native `cargo test` may fail on hosts without a system linker. The documented Docker workflow is the supported path for local verification.

## Configuration

Client defaults live in `.env.client`:

```env
OPENROK_SERVER=http://127.0.0.1:8080
```

Server defaults live in `.env.server`:

```env
OPENROK_BIND=127.0.0.1:8080
OPENROK_DOMAIN=aturl.xyz
OPENROK_CREATE_LIMIT_PER_MINUTE=20
```

## Run The Relay

```bash
docker run --rm -it \
  -v "$PWD":/workspace \
  -w /workspace \
  -p 8080:8080 \
  rust:1.94 \
  cargo run -p server -- --bind 0.0.0.0:8080 --domain aturl.xyz
```

Or use Docker Compose:

```bash
docker compose up
```

The compose stack runs:

- `server`: the Rust relay on the internal Docker network, started with `cargo run -p server`
- `traefik`: reverse proxy in front of the relay on ports `80` and `443`, using a rendered file-based config

The compose file reads server settings from `.env.server`.

Traefik explicitly routes `relay.aturl.xyz` and `*.aturl.xyz` to `http://server:8080` from a rendered config file. The relay still validates `OPENROK_DOMAIN`, so only matching hosts are accepted by the app.

Traefik loads a Cloudflare origin certificate from `certs/<domain>.crt` and `certs/<domain>.key`, using the domain from `.env.server`.

Important:
- the domain in `.env.server` must match the cert filenames
- Cloudflare origin certs are meant for Cloudflare-to-origin traffic, not direct browser trust
- if requests go through Cloudflare proxy, use SSL mode `Full (strict)`

For local DNS, add at least:

```text
127.0.0.1 relay.aturl.xyz
```

Wildcard subdomains such as `demo.aturl.xyz` still require wildcard DNS or another local DNS solution. `/etc/hosts` is not enough for arbitrary tunnel subdomains.

## Run The Client

Start your local app first, then run:

```bash
docker run --rm -it \
  --network host \
  -v "$PWD":/workspace \
  -w /workspace \
  rust:1.94 \
  cargo run -p client -- --server https://relay.aturl.xyz http 3000 --subdomain demo
```

## Test The Tunnel

```bash
curl -H 'Host: demo.aturl.xyz' http://127.0.0.1:8080/
```

The relay uses the `Host` header to select the tunnel and forwards the request to the client, which proxies it to `http://127.0.0.1:3000`.

## Run Tests

```bash
docker run --rm \
  -v "$PWD":/workspace \
  -w /workspace \
  rust:1.94 \
  cargo test --workspace
```

## Notes

- Tunnel creation is open and does not require auth.
- The relay enforces a basic per-IP tunnel creation rate limit.
- The current implementation is a strong MVP baseline, not a full hosted production service. Wildcard DNS/TLS, websocket upgrade forwarding, large-body streaming, and auth are still future work.
