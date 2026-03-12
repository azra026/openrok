use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use openrok_shared::protocol::{
    ClientRegistered, ControlMessage, CreateTunnelRequest, ForwardRequest, ForwardResponse, Header,
    Heartbeat, RegisterClient, TunnelCreated, TunnelProtocol, decode_body, encode_body,
};
use reqwest::Method;
use tokio::time::{Duration, interval};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

type ControlSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug, Parser)]
#[command(name = "rok", version, about = "Expose localhost through OpenRok")]
struct Cli {
    #[arg(long)]
    server: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Http {
        port: u16,
        #[arg(long)]
        subdomain: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::from_filename(".env.client").or_else(|_| dotenvy::dotenv());
    init_tracing();

    let cli = Cli::parse();
    let server = cli
        .server
        .or_else(|| env::var("OPENROK_SERVER").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    match cli.command {
        Command::Http { port, subdomain } => run_http(&server, port, subdomain).await,
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "client=info".into()),
        )
        .with_target(false)
        .compact()
        .init();
}

async fn run_http(server: &str, port: u16, subdomain: Option<String>) -> Result<()> {
    info!(server, port, subdomain = ?subdomain, "requesting tunnel");
    let request = CreateTunnelRequest {
        protocol: TunnelProtocol::Http,
        port,
        subdomain,
    };
    let client = reqwest::Client::new();
    let endpoint = format!("{}/tunnels", server.trim_end_matches('/'));
    let response = client
        .post(endpoint)
        .json(&request)
        .send()
        .await
        .context("failed to contact relay server")?
        .error_for_status()
        .context("relay server returned an error")?;
    let message = response
        .json::<ControlMessage>()
        .await
        .context("failed to decode relay response")?;

    let created = parse_created_message(message)?;
    println!("OpenRok v0.1.0");
    println!();
    println!("Session Status: Online");
    println!();
    println!("Tunnel ID: {}", created.tunnel_id);
    println!("Forwarding");
    println!("{} -> {}", created.url, created.local_addr);
    println!("Local browser");
    println!("{}", local_browser_url(server, &created.url));
    println!("Local test");
    println!(
        "curl -H 'Host: {}' {}/",
        public_host(&created.url),
        server.trim_end_matches('/')
    );
    println!("Note");
    println!("The public URL needs DNS/TLS to work directly in a browser.");

    establish_control_channel(server, &created).await?;

    Ok(())
}

async fn establish_control_channel(server: &str, created: &TunnelCreated) -> Result<()> {
    let websocket_url = build_websocket_url(server);
    info!(websocket_url, tunnel_id = %created.tunnel_id, "opening control channel");
    let (mut socket, _) = connect_async(websocket_url)
        .await
        .context("failed to open control channel")?;

    let register = ControlMessage::RegisterClient(RegisterClient {
        tunnel_id: created.tunnel_id.clone(),
        local_addr: created.local_addr.clone(),
        registration_token: created.registration_token.clone(),
    });
    send_control_message(&mut socket, &register).await?;

    let registration_ack = read_control_message(&mut socket).await?;
    ensure_registered(registration_ack, &created.tunnel_id)?;
    info!(tunnel_id = %created.tunnel_id, "relay confirmed client registration");

    let http_client = reqwest::Client::new();
    let mut ticker = interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                debug!(tunnel_id = %created.tunnel_id, "sending heartbeat");
                let heartbeat = ControlMessage::Heartbeat(Heartbeat {
                    tunnel_id: created.tunnel_id.clone(),
                });
                send_control_message(&mut socket, &heartbeat).await?;
            }
            incoming = socket.next() => {
                let Some(message) = incoming else {
                    anyhow::bail!("control channel closed unexpectedly");
                };
                let message = message.context("control channel closed unexpectedly")?;
                if let Some(reply) = handle_incoming_message(&http_client, &created.local_addr, &created.tunnel_id, message).await? {
                    send_control_message(&mut socket, &reply).await?;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!(tunnel_id = %created.tunnel_id, "received shutdown signal");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_incoming_message(
    http_client: &reqwest::Client,
    local_addr: &str,
    tunnel_id: &str,
    message: Message,
) -> Result<Option<ControlMessage>> {
    let Message::Text(payload) = message else {
        return Ok(None);
    };

    let control_message: ControlMessage =
        serde_json::from_str(&payload).context("failed to decode control message")?;

    match control_message {
        ControlMessage::ForwardRequest(request) => {
            let request_id = request.request_id.clone();
            info!(
                tunnel_id,
                request_id = %request_id,
                method = %request.method,
                path = %request.path,
                query = ?request.query,
                "received forwarded request"
            );
            let response = match proxy_local_request(http_client, local_addr, request).await {
                Ok(response) => {
                    info!(
                        tunnel_id,
                        request_id = %response.request_id,
                        status = response.status,
                        "proxied local request"
                    );
                    response
                }
                Err(error) => {
                    warn!(
                        tunnel_id,
                        request_id = %request_id,
                        error = %error,
                        "local proxy request failed"
                    );
                    ForwardResponse {
                        request_id,
                        status: 502,
                        headers: vec![Header {
                            name: "content-type".to_string(),
                            value: "text/plain".to_string(),
                        }],
                        body_base64: encode_body(format!("proxy error: {error}").as_bytes()),
                    }
                }
            };
            Ok(Some(ControlMessage::ForwardResponse(response)))
        }
        ControlMessage::Heartbeat(_) => {
            debug!(tunnel_id, "received heartbeat acknowledgement");
            Ok(None)
        }
        ControlMessage::ClientRegistered(ClientRegistered {
            tunnel_id: registered_id,
        }) if registered_id == tunnel_id => {
            debug!(tunnel_id, "received duplicate registration confirmation");
            Ok(None)
        }
        _ => Ok(None),
    }
}

async fn proxy_local_request(
    http_client: &reqwest::Client,
    local_addr: &str,
    request: ForwardRequest,
) -> Result<ForwardResponse> {
    let body = decode_body(&request.body_base64).context("failed to decode forwarded body")?;
    let method = Method::from_bytes(request.method.as_bytes())
        .context("failed to parse forwarded method")?;
    let mut url = format!(
        "{}{}",
        local_addr.trim_end_matches('/'),
        normalize_path(&request.path)
    );
    if let Some(query) = request.query.as_deref() {
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
    }

    let mut builder = http_client.request(method, url).body(body);
    for header in &request.headers {
        builder = builder.header(&header.name, &header.value);
    }

    let response = builder
        .send()
        .await
        .context("failed to proxy request to localhost")?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some(Header {
                name: name.as_str().to_string(),
                value: value.to_str().ok()?.to_string(),
            })
        })
        .collect();
    let body = response
        .bytes()
        .await
        .context("failed to read local response body")?;

    Ok(ForwardResponse {
        request_id: request.request_id,
        status,
        headers,
        body_base64: encode_body(&body),
    })
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn build_websocket_url(server: &str) -> String {
    if let Some(stripped) = server.strip_prefix("https://") {
        return format!("wss://{}/ws", stripped.trim_end_matches('/'));
    }

    let stripped = server.trim_start_matches("http://").trim_end_matches('/');
    format!("ws://{stripped}/ws")
}

fn public_host(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

fn local_browser_url(server: &str, public_url: &str) -> String {
    format!(
        "{}/_openrok/{}/",
        server.trim_end_matches('/'),
        public_host(public_url)
    )
}

fn parse_created_message(message: ControlMessage) -> Result<TunnelCreated> {
    match message {
        ControlMessage::TunnelCreated(created) => Ok(created),
        ControlMessage::CreateTunnel(_) => {
            anyhow::bail!("relay server returned an unexpected message")
        }
        ControlMessage::ClientRegistered(_) => {
            anyhow::bail!("relay server returned an unexpected registration message")
        }
        ControlMessage::RegisterClient(_) => {
            anyhow::bail!("relay server returned an invalid request echo")
        }
        ControlMessage::Heartbeat(_) => {
            anyhow::bail!("relay server returned a heartbeat before registration")
        }
        ControlMessage::ForwardRequest(_) | ControlMessage::ForwardResponse(_) => {
            anyhow::bail!("relay server returned a forwarding message before registration")
        }
    }
}

fn ensure_registered(message: ControlMessage, tunnel_id: &str) -> Result<()> {
    match message {
        ControlMessage::ClientRegistered(ClientRegistered {
            tunnel_id: registered_id,
        }) if registered_id == tunnel_id => Ok(()),
        _ => anyhow::bail!("relay server failed to confirm client registration"),
    }
}

async fn send_control_message(socket: &mut ControlSocket, message: &ControlMessage) -> Result<()> {
    let payload = serde_json::to_string(message).context("failed to encode control message")?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .context("failed to send control message")
}

async fn read_control_message(socket: &mut ControlSocket) -> Result<ControlMessage> {
    while let Some(message) = socket.next().await {
        let message = message.context("control channel closed unexpectedly")?;
        if let Message::Text(payload) = message {
            return serde_json::from_str(&payload).context("failed to decode control message");
        }
    }

    anyhow::bail!("control channel closed unexpectedly")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tunnel_created_message() {
        let message = ControlMessage::TunnelCreated(TunnelCreated {
            tunnel_id: "tnl_demo".to_string(),
            url: "https://demo.openrok.test".to_string(),
            local_addr: "http://127.0.0.1:3000".to_string(),
            registration_token: "tok_demo".to_string(),
        });

        let created = parse_created_message(message).expect("message should parse");

        assert_eq!(created.tunnel_id, "tnl_demo");
        assert_eq!(created.url, "https://demo.openrok.test");
    }

    #[test]
    fn builds_local_browser_url() {
        assert_eq!(
            local_browser_url("http://127.0.0.1:8080", "https://demo.openrok.test"),
            "http://127.0.0.1:8080/_openrok/demo.openrok.test/"
        );
    }

    #[test]
    fn rejects_unexpected_message_variant() {
        let message = ControlMessage::CreateTunnel(CreateTunnelRequest {
            protocol: TunnelProtocol::Http,
            port: 3000,
            subdomain: None,
        });

        let error = parse_created_message(message).expect_err("message should be rejected");

        assert!(error.to_string().contains("unexpected message"));
    }

    #[test]
    fn builds_websocket_url_from_http_server() {
        assert_eq!(
            build_websocket_url("http://127.0.0.1:8080"),
            "ws://127.0.0.1:8080/ws"
        );
    }

    #[test]
    fn accepts_matching_registration_ack() {
        let result = ensure_registered(
            ControlMessage::ClientRegistered(ClientRegistered {
                tunnel_id: "tnl_demo".to_string(),
            }),
            "tnl_demo",
        );

        assert!(result.is_ok());
    }

    #[test]
    fn normalizes_forwarded_path() {
        assert_eq!(normalize_path("users"), "/users");
        assert_eq!(normalize_path("/users"), "/users");
    }

    #[test]
    fn extracts_public_host_from_url() {
        assert_eq!(
            public_host("https://demo.openrok.test"),
            "demo.openrok.test"
        );
    }
}
