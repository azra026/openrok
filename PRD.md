# Product Requirements Document (PRD)

# OpenRok

**Open source tunneling tool for exposing localhost to the internet.**

---

# 1. Product Overview

OpenRok is a **Rust-based tunneling tool** that allows developers to expose local services to the public internet through a secure tunnel.

Developers can run a simple CLI command:

```bash
rok http 3000
```

OpenRok returns a public URL:

```
https://abc123.openrok.io → http://localhost:3000
```

Requests sent to the public URL are forwarded to the developer’s local machine.

---

# 2. Problem

Developers often need to expose a local server to the internet for:

* webhook testing
* sharing local demos
* mobile testing
* remote debugging
* temporary public access

Deploying to cloud infrastructure for this purpose is slow and inconvenient.

OpenRok solves this by creating **instant public tunnels to localhost**.

---

# 3. Target Users

Primary users:

* software developers
* backend engineers
* API developers
* DevOps engineers

Typical workflow:

```
run local server
↓
run openrok
↓
share public URL
```

Example:

```
rok http 3000
```

---

# 4. Core User Experience

### Example CLI Usage

User runs:

```bash
rok http 3000
```

CLI output:

```
OpenRok v0.1

Session Status: Online

Forwarding
https://blue-dog-81.openrok.io -> http://localhost:3000
```

Now any request to the public URL will be forwarded to the local server.

---

# 5. Key Features (MVP)

## 1. CLI Client

A command-line interface used by developers.

Example command:

```bash
rok http 80
```

Responsibilities:

* connect to OpenRok server
* request tunnel
* proxy requests to localhost
* maintain persistent connection

---

## 2. Relay Server

The OpenRok server runs on a public VPS.

Responsibilities:

* manage tunnel sessions
* generate public URLs
* receive public HTTP requests
* route traffic to the correct client

---

## 3. Public URL Generation

The server generates a random subdomain for each tunnel.

Example:

```
abc123.openrok.io
```

DNS configuration required:

```
*.openrok.io → server IP
```

This allows unlimited dynamic subdomains.

---

## 4. HTTP Request Forwarding

Traffic flow:

```
Browser
↓
OpenRok Server
↓
Tunnel Connection
↓
OpenRok Client
↓
localhost
```

The response travels back through the same tunnel.

---

# 6. CLI Commands

### HTTP Tunnel

```
rok http <port>
```

Example:

```
rok http 3000
```

---

### Custom Subdomain (Optional)

```
rok http 3000 --subdomain myapp
```

Result:

```
https://myapp.openrok.io
```

---

### Authentication

```
rok auth <token>
```

Stores authentication credentials locally.

---

# 7. System Architecture

OpenRok uses a **client-server architecture**.

```
             Internet
                │
                ▼
       ┌──────────────────┐
       │   OpenRok Server │
       │    (Public VPS)  │
       └──────────────────┘
                │
          Persistent Tunnel
                │
                ▼
       ┌──────────────────┐
       │   OpenRok Client │
       │      (CLI)       │
       └──────────────────┘
                │
                ▼
           localhost:PORT
```

---

# 8. Technologies

### Language

Rust

### Async runtime

```
tokio
```

### HTTP server

```
axum
or
hyper
```

### Tunnel connection

```
tokio-tungstenite (WebSocket)
```

### TLS

```
rustls
```

### CLI parsing

```
clap
```

### Serialization

```
serde
```

### Logging

```
tracing
```

---

# 9. Tunnel Protocol (Simplified)

Client and server communicate using control messages.

Example tunnel request:

```json
{
  "type": "create_tunnel",
  "protocol": "http",
  "port": 3000
}
```

Server response:

```json
{
  "type": "tunnel_created",
  "url": "https://abc123.openrok.io"
}
```

Forwarded request:

```json
{
  "type": "http_request",
  "id": "req123",
  "method": "GET",
  "path": "/"
}
```

---

# 10. MVP Scope

The MVP must include:

* CLI client
* relay server
* HTTP tunneling
* random subdomain generation
* request forwarding
* persistent connection
* simple logging

---

# 11. Out of Scope (MVP)

The following features will NOT be included initially:

* dashboard
* billing system
* traffic analytics
* request inspection
* distributed edge network

---

# 12. Project Structure

Recommended Rust workspace:

```
openrok/
│
├── openrok-server/
│   └── src/
│       ├── main.rs
│       ├── tunnel_manager.rs
│       ├── router.rs
│       └── protocol.rs
│
├── openrok-client/
│   └── src/
│       ├── main.rs
│       ├── tunnel.rs
│       ├── proxy.rs
│       └── protocol.rs
│
└── shared/
    └── protocol.rs
```

---

# 13. Development Milestones

### Milestone 1 — Basic Client Connection

Goal:

Client connects to server.

Features:

* WebSocket connection
* basic handshake

---

### Milestone 2 — Tunnel Creation

Goal:

Server generates a public URL.

Example:

```
abc123.openrok.io
```

---

### Milestone 3 — HTTP Forwarding

Goal:

Public requests are forwarded through the tunnel to localhost.

---

### Milestone 4 — CLI Improvements

Add:

* session status
* forwarding URL
* logs

---

### Milestone 5 — Stability

Add:

* heartbeat system
* reconnect logic
* improved error handling

---

# 14. Success Criteria

The MVP is successful if a developer can run:

```
rok http 3000
```

Receive a public URL and access their local service from the internet.