mod tunnel_manager;

use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::Context;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{
        ConnectInfo, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use openrok_shared::protocol::{
    ClientRegistered, ControlMessage, CreateTunnelRequest, ForwardRequest, ForwardResponse, Header,
    TunnelProtocol, decode_body, encode_body,
};
use tokio::sync::Mutex;
use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tracing::info;

use crate::tunnel_manager::CreateTunnelError;
use crate::tunnel_manager::TunnelManager;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct AppState {
    tunnel_manager: TunnelManager,
    create_limiter: CreateRateLimiter,
}

#[derive(Debug, Parser)]
#[command(name = "openrok-server", about = "OpenRok relay server")]
struct ServerCli {
    #[arg(long)]
    bind: Option<SocketAddr>,
    #[arg(long)]
    domain: Option<String>,
    #[arg(long)]
    create_limit_per_minute: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::from_filename(".env.server").or_else(|_| dotenvy::dotenv());
    init_tracing();
    let cli = ServerCli::parse();
    let bind = cli
        .bind
        .or_else(|| env::var("OPENROK_BIND").ok()?.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:8080".parse().expect("default bind should parse"));
    let domain = cli
        .domain
        .or_else(|| env::var("OPENROK_DOMAIN").ok())
        .unwrap_or_else(|| "openrok.test".to_string());
    let create_limit_per_minute = cli
        .create_limit_per_minute
        .or_else(|| {
            env::var("OPENROK_CREATE_LIMIT_PER_MINUTE")
                .ok()?
                .parse()
                .ok()
        })
        .unwrap_or(20);

    let state = AppState {
        tunnel_manager: TunnelManager::new(&domain),
        create_limiter: CreateRateLimiter::new(create_limit_per_minute, Duration::from_secs(60)),
    };
    spawn_session_reaper(state.tunnel_manager.clone());

    let app = Router::new()
        .route("/health", get(health))
        .route("/tunnels", post(create_tunnel))
        .route("/ws", get(websocket_upgrade))
        .fallback(host_proxy_request)
        .with_state(state);

    let listener = TcpListener::bind(bind)
        .await
        .context("failed to bind relay server")?;

    info!("server listening on http://{}", bind);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("relay server exited unexpectedly")?;

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "server=info".into()),
        )
        .with_target(false)
        .compact()
        .init();
}

async fn health() -> &'static str {
    "ok"
}

async fn create_tunnel(
    State(state): State<AppState>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    axum::Json(request): axum::Json<CreateTunnelRequest>,
) -> impl IntoResponse {
    let remote_ip = remote_addr.ip().to_string();
    if !state.create_limiter.allow(&remote_ip).await {
        return text_response(
            StatusCode::TOO_MANY_REQUESTS,
            "tunnel creation rate limit exceeded",
        );
    }

    match state.tunnel_manager.create_tunnel(request).await {
        Ok(created) => (
            StatusCode::CREATED,
            axum::Json(ControlMessage::TunnelCreated(created)),
        )
            .into_response(),
        Err(CreateTunnelError::InvalidSubdomain) => {
            text_response(StatusCode::BAD_REQUEST, "invalid subdomain")
        }
        Err(CreateTunnelError::SubdomainUnavailable) => {
            text_response(StatusCode::CONFLICT, "subdomain unavailable")
        }
    }
}

async fn host_proxy_request(State(state): State<AppState>, request: Request) -> Response<Body> {
    match proxy_request_inner(state, request).await {
        Ok(response) => response,
        Err((status, message)) => text_response(status, message),
    }
}

async fn proxy_request_inner(
    state: AppState,
    request: Request,
) -> Result<Response<Body>, (StatusCode, String)> {
    let (parts, body) = request.into_parts();
    let host = host_from_headers(&parts.headers).ok_or((
        StatusCode::BAD_REQUEST,
        "missing or invalid host header".to_string(),
    ))?;
    let tunnel_id = state.tunnel_manager.tunnel_id_for_host(&host).ok_or((
        StatusCode::BAD_REQUEST,
        "missing or invalid host header".to_string(),
    ))?;
    let body = to_bytes(body, 1024 * 1024).await.map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "failed to read request body".to_string(),
        )
    })?;
    let request_id = next_request_id();
    let query = parts.uri.query().map(ToString::to_string);
    let headers = forwarded_headers(&parts.headers, &host);
    let message = ControlMessage::ForwardRequest(ForwardRequest {
        tunnel_id: tunnel_id.clone(),
        request_id: request_id.clone(),
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query,
        headers,
        body_base64: encode_body(&body),
    });

    let response_rx = state
        .tunnel_manager
        .dispatch_request(&tunnel_id, request_id, message)
        .await
        .map_err(|error| (StatusCode::NOT_FOUND, error.to_string()))?;

    let forwarded = timeout(Duration::from_secs(10), response_rx)
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                "tunnel request timed out".to_string(),
            )
        })?
        .map_err(|_| (StatusCode::BAD_GATEWAY, "client disconnected".to_string()))?;

    Ok(build_proxy_response(forwarded))
}

async fn websocket_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (sender, mut receiver) = mpsc::unbounded_channel::<ControlMessage>();
    let mut registered_tunnel_id: Option<String> = None;

    let writer = tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            let Ok(payload) = serde_json::to_string(&message) else {
                continue;
            };

            if sink.send(Message::Text(payload.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(message_result) = stream.next().await {
        let Ok(message) = message_result else {
            break;
        };

        let Message::Text(payload) = message else {
            continue;
        };

        let Ok(control_message) = serde_json::from_str::<ControlMessage>(&payload) else {
            continue;
        };

        match control_message {
            ControlMessage::RegisterClient(register) => {
                match state
                    .tunnel_manager
                    .register_client(
                        register.tunnel_id.clone(),
                        register.local_addr,
                        register.registration_token,
                        sender.clone(),
                    )
                    .await
                {
                    Ok(()) => {
                        registered_tunnel_id = Some(register.tunnel_id.clone());
                        let _ = sender.send(ControlMessage::ClientRegistered(ClientRegistered {
                            tunnel_id: register.tunnel_id,
                        }));
                    }
                    Err(_) => break,
                }
            }
            ControlMessage::Heartbeat(heartbeat) => {
                if state
                    .tunnel_manager
                    .record_heartbeat(&heartbeat.tunnel_id)
                    .await
                {
                    let _ = sender.send(ControlMessage::Heartbeat(heartbeat));
                }
            }
            ControlMessage::ForwardResponse(response) => {
                let _ = state.tunnel_manager.resolve_response(response).await;
            }
            _ => {}
        }
    }

    writer.abort();
    if let Some(tunnel_id) = registered_tunnel_id {
        state.tunnel_manager.remove_client(&tunnel_id).await;
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn spawn_session_reaper(tunnel_manager: TunnelManager) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            tunnel_manager
                .prune_stale_sessions(Duration::from_secs(90))
                .await;
            tunnel_manager.prune_stale_reservations().await;
        }
    });
}

fn headers_to_protocol<'a>(
    headers: impl Iterator<Item = (&'a HeaderName, &'a HeaderValue)>,
) -> Vec<Header> {
    headers
        .filter_map(|(name, value)| {
            Some(Header {
                name: name.as_str().to_string(),
                value: value.to_str().ok()?.to_string(),
            })
        })
        .collect()
}

fn forwarded_headers(headers: &HeaderMap, host: &str) -> Vec<Header> {
    let mut headers = headers_to_protocol(headers.iter());
    headers.retain(|header| !matches!(header.name.as_str(), "host" | "connection"));
    headers.push(Header {
        name: "x-forwarded-host".to_string(),
        value: host.to_string(),
    });
    headers.push(Header {
        name: "x-forwarded-proto".to_string(),
        value: "http".to_string(),
    });
    headers.push(Header {
        name: "x-forwarded-for".to_string(),
        value: "127.0.0.1".to_string(),
    });
    headers
}

fn host_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::HOST)?
        .to_str()
        .ok()
        .map(ToString::to_string)
}

fn build_proxy_response(response: ForwardResponse) -> Response<Body> {
    let body = decode_body(&response.body_base64).unwrap_or_default();
    let mut builder = Response::builder().status(response.status);
    for header in response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(header.name.as_bytes()),
            HeaderValue::from_str(&header.value),
        ) {
            builder = builder.header(name, value);
        }
    }

    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| text_response(StatusCode::BAD_GATEWAY, "invalid client response"))
}

fn text_response(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(body.into()))
        .expect("text response should build")
}

fn next_request_id() -> String {
    format!("req_{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone)]
struct CreateRateLimiter {
    limit: usize,
    window: Duration,
    entries: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl CreateRateLimiter {
    fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        let timestamps = entries.entry(key.to_string()).or_default();
        timestamps.retain(|timestamp| now.duration_since(*timestamp) < self.window);

        if timestamps.len() >= self.limit {
            return false;
        }

        timestamps.push(now);
        true
    }
}

#[allow(dead_code)]
fn _protocol_example() -> ControlMessage {
    ControlMessage::CreateTunnel(CreateTunnelRequest {
        protocol: TunnelProtocol::Http,
        port: 3000,
        subdomain: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_check_returns_ok() {
        let response = health().await;

        assert_eq!(response, "ok");
    }

    #[test]
    fn creates_tunnel_url_from_request() {
        let state = AppState {
            tunnel_manager: TunnelManager::new("openrok.test"),
            create_limiter: CreateRateLimiter::new(20, Duration::from_secs(60)),
        };
        let request = CreateTunnelRequest {
            protocol: TunnelProtocol::Http,
            port: 3000,
            subdomain: Some("demo".to_string()),
        };

        let created = tokio::runtime::Runtime::new()
            .expect("runtime should create")
            .block_on(state.tunnel_manager.create_tunnel(request))
            .expect("tunnel should create");

        assert_eq!(created.url, "https://demo.openrok.test");
        assert_eq!(created.tunnel_id, "tnl_demo");
        assert_eq!(created.local_addr, "http://127.0.0.1:3000");
        assert!(!created.registration_token.is_empty());
    }

    #[test]
    fn builds_proxy_response_from_forwarded_response() {
        let response = build_proxy_response(ForwardResponse {
            request_id: "req_1".to_string(),
            status: 201,
            headers: vec![Header {
                name: "content-type".to_string(),
                value: "text/plain".to_string(),
            }],
            body_base64: encode_body(b"created"),
        });

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[test]
    fn resolves_tunnel_id_from_host_header() {
        let state = AppState {
            tunnel_manager: TunnelManager::new("openrok.test"),
            create_limiter: CreateRateLimiter::new(20, Duration::from_secs(60)),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "demo.openrok.test".parse().unwrap(),
        );

        let tunnel_id = state
            .tunnel_manager
            .tunnel_id_for_host(&host_from_headers(&headers).unwrap());

        assert_eq!(tunnel_id, Some("tnl_demo".to_string()));
    }

    #[test]
    fn adds_forwarded_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "demo.openrok.test".parse().unwrap(),
        );
        headers.insert("user-agent", "curl/8.0".parse().unwrap());

        let forwarded = forwarded_headers(&headers, "demo.openrok.test");

        assert!(
            forwarded
                .iter()
                .any(|header| header.name == "x-forwarded-host")
        );
        assert!(
            forwarded
                .iter()
                .any(|header| header.name == "x-forwarded-proto")
        );
        assert!(forwarded.iter().all(|header| header.name != "host"));
    }

    #[tokio::test]
    async fn rate_limiter_blocks_after_limit() {
        let limiter = CreateRateLimiter::new(2, Duration::from_secs(60));

        assert!(limiter.allow("127.0.0.1").await);
        assert!(limiter.allow("127.0.0.1").await);
        assert!(!limiter.allow("127.0.0.1").await);
    }
}
