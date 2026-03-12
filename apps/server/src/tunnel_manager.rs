use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use openrok_shared::protocol::{
    ControlMessage, CreateTunnelRequest, ForwardResponse, TunnelCreated,
};
use rand::{Rng, distr::Alphanumeric, rng};
use tokio::sync::{Mutex, mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct ClientSession {
    pub local_addr: String,
    pub public_host: String,
    pub last_seen: Instant,
    pub sender: mpsc::UnboundedSender<ControlMessage>,
}

#[derive(Debug, Clone)]
struct TunnelReservation {
    public_host: String,
    local_addr: String,
    registration_token: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateTunnelError {
    InvalidSubdomain,
    SubdomainUnavailable,
}

#[derive(Clone)]
pub struct TunnelManager {
    domain: String,
    sessions: Arc<Mutex<HashMap<String, ClientSession>>>,
    reservations: Arc<Mutex<HashMap<String, TunnelReservation>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ForwardResponse>>>>,
}

impl TunnelManager {
    pub fn new(domain: &str) -> Self {
        Self {
            domain: domain.to_string(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            reservations: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn create_tunnel(
        &self,
        request: CreateTunnelRequest,
    ) -> Result<TunnelCreated, CreateTunnelError> {
        let subdomain = match request.subdomain {
            Some(subdomain) => {
                let normalized =
                    normalize_subdomain(&subdomain).ok_or(CreateTunnelError::InvalidSubdomain)?;
                if !is_valid_subdomain(&normalized) {
                    return Err(CreateTunnelError::InvalidSubdomain);
                }
                normalized
            }
            None => self.generate_unique_subdomain().await,
        };

        let public_host = format!("{}.{}", subdomain, self.domain);
        if self.host_in_use(&public_host).await {
            return Err(CreateTunnelError::SubdomainUnavailable);
        }

        let tunnel_id = format!("tnl_{}", subdomain.replace('-', "_"));
        let registration_token = random_token(24);
        let local_addr = format!("http://127.0.0.1:{}", request.port);
        let reservation = TunnelReservation {
            public_host: public_host.clone(),
            local_addr: local_addr.clone(),
            registration_token: registration_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(60),
        };

        self.reservations
            .lock()
            .await
            .insert(tunnel_id.clone(), reservation);

        Ok(TunnelCreated {
            tunnel_id,
            url: format!("https://{public_host}"),
            local_addr,
            registration_token,
        })
    }

    pub fn tunnel_id_for_host(&self, host: &str) -> Option<String> {
        let normalized = host.split(':').next()?.trim().to_ascii_lowercase();
        let domain = self.domain.to_ascii_lowercase();
        let suffix = format!(".{domain}");

        if !normalized.ends_with(&suffix) {
            return None;
        }

        let subdomain = normalized.strip_suffix(&suffix)?;
        if subdomain.is_empty() {
            return None;
        }

        Some(format!("tnl_{}", subdomain.replace('-', "_")))
    }

    pub async fn register_client(
        &self,
        tunnel_id: String,
        local_addr: String,
        registration_token: String,
        sender: mpsc::UnboundedSender<ControlMessage>,
    ) -> Result<(), anyhow::Error> {
        let reservation = self
            .reservations
            .lock()
            .await
            .remove(&tunnel_id)
            .ok_or_else(|| anyhow!("no pending tunnel reservation found"))?;

        if reservation.registration_token != registration_token {
            return Err(anyhow!("invalid registration token"));
        }

        if reservation.expires_at < Instant::now() {
            return Err(anyhow!("tunnel reservation expired"));
        }

        if reservation.local_addr != local_addr {
            return Err(anyhow!("local address does not match reserved tunnel"));
        }

        let session = ClientSession {
            local_addr: reservation.local_addr,
            public_host: reservation.public_host,
            last_seen: Instant::now(),
            sender,
        };

        self.sessions.lock().await.insert(tunnel_id, session);
        Ok(())
    }

    pub async fn remove_client(&self, tunnel_id: &str) {
        self.sessions.lock().await.remove(tunnel_id);
    }

    pub async fn dispatch_request(
        &self,
        tunnel_id: &str,
        request_id: String,
        message: ControlMessage,
    ) -> Result<oneshot::Receiver<ForwardResponse>> {
        let sender = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(tunnel_id)
                .ok_or_else(|| anyhow!("no active tunnel session found"))?;
            session.sender.clone()
        };

        let (response_tx, response_rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id.clone(), response_tx);
        if sender.send(message).is_err() {
            self.pending.lock().await.remove(&request_id);
            return Err(anyhow!("failed to send forwarded request to client"));
        }

        Ok(response_rx)
    }

    pub async fn resolve_response(&self, response: ForwardResponse) -> bool {
        let sender = self.pending.lock().await.remove(&response.request_id);
        let Some(sender) = sender else {
            return false;
        };

        sender.send(response).is_ok()
    }

    pub async fn record_heartbeat(&self, tunnel_id: &str) -> bool {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(tunnel_id) else {
            return false;
        };

        session.last_seen = Instant::now();
        true
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    pub async fn prune_stale_sessions(&self, max_age: Duration) {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| session.last_seen.elapsed() <= max_age);
    }

    pub async fn prune_stale_reservations(&self) {
        let mut reservations = self.reservations.lock().await;
        reservations.retain(|_, reservation| reservation.expires_at > Instant::now());
    }

    pub async fn client_addr(&self, tunnel_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(tunnel_id)
            .map(|session| session.local_addr.clone())
    }

    async fn generate_unique_subdomain(&self) -> String {
        loop {
            let candidate = format!("rok-{}", random_token(8).to_ascii_lowercase());
            let public_host = format!("{}.{}", candidate, self.domain);
            if !self.host_in_use(&public_host).await {
                return candidate;
            }
        }
    }

    async fn host_in_use(&self, public_host: &str) -> bool {
        if self
            .sessions
            .lock()
            .await
            .values()
            .any(|session| session.public_host == public_host)
        {
            return true;
        }

        self.reservations
            .lock()
            .await
            .values()
            .any(|reservation| reservation.public_host == public_host)
    }
}

fn random_token(length: usize) -> String {
    rng()
        .sample_iter(Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn normalize_subdomain(subdomain: &str) -> Option<String> {
    let normalized = subdomain.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

fn is_valid_subdomain(subdomain: &str) -> bool {
    let bytes = subdomain.as_bytes();
    if !(3..=63).contains(&bytes.len()) {
        return false;
    }
    if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
        return false;
    }

    bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrok_shared::protocol::{ForwardRequest, Header, TunnelProtocol, encode_body};

    #[tokio::test]
    async fn generates_default_subdomain_from_port() {
        let manager = TunnelManager::new("openrok.test");
        let request = CreateTunnelRequest {
            protocol: TunnelProtocol::Http,
            port: 8080,
            subdomain: None,
        };

        let created = manager
            .create_tunnel(request)
            .await
            .expect("tunnel should create");

        assert!(created.tunnel_id.starts_with("tnl_rok_"));
        assert!(created.url.starts_with("https://rok-"));
        assert_eq!(created.local_addr, "http://127.0.0.1:8080");
        assert!(!created.registration_token.is_empty());
    }

    #[tokio::test]
    async fn rejects_invalid_subdomain() {
        let manager = TunnelManager::new("openrok.test");
        let request = CreateTunnelRequest {
            protocol: TunnelProtocol::Http,
            port: 8080,
            subdomain: Some("Invalid_Subdomain".to_string()),
        };

        let error = manager
            .create_tunnel(request)
            .await
            .expect_err("should fail");

        assert_eq!(error, CreateTunnelError::InvalidSubdomain);
    }

    #[tokio::test]
    async fn registers_and_counts_client_session() {
        let manager = TunnelManager::new("openrok.test");
        let created = manager
            .create_tunnel(CreateTunnelRequest {
                protocol: TunnelProtocol::Http,
                port: 3000,
                subdomain: Some("demo".to_string()),
            })
            .await
            .expect("tunnel should create");
        let (sender, _receiver) = mpsc::unbounded_channel();

        manager
            .register_client(
                created.tunnel_id.clone(),
                "http://127.0.0.1:3000".to_string(),
                created.registration_token,
                sender,
            )
            .await
            .expect("registration should succeed");

        assert_eq!(manager.session_count().await, 1);
        assert!(manager.record_heartbeat(&created.tunnel_id).await);
        assert_eq!(
            manager.client_addr(&created.tunnel_id).await,
            Some("http://127.0.0.1:3000".to_string())
        );

        manager.remove_client(&created.tunnel_id).await;
        assert_eq!(manager.session_count().await, 0);
    }

    #[tokio::test]
    async fn rejects_invalid_registration_token() {
        let manager = TunnelManager::new("openrok.test");
        let created = manager
            .create_tunnel(CreateTunnelRequest {
                protocol: TunnelProtocol::Http,
                port: 3000,
                subdomain: Some("demo".to_string()),
            })
            .await
            .expect("tunnel should create");
        let (sender, _receiver) = mpsc::unbounded_channel();

        let error = manager
            .register_client(
                created.tunnel_id,
                "http://127.0.0.1:3000".to_string(),
                "wrong-token".to_string(),
                sender,
            )
            .await
            .expect_err("registration should fail");

        assert!(error.to_string().contains("invalid registration token"));
    }

    #[tokio::test]
    async fn rejects_duplicate_subdomain_reservation() {
        let manager = TunnelManager::new("openrok.test");

        manager
            .create_tunnel(CreateTunnelRequest {
                protocol: TunnelProtocol::Http,
                port: 3000,
                subdomain: Some("demo".to_string()),
            })
            .await
            .expect("first tunnel should create");

        let error = manager
            .create_tunnel(CreateTunnelRequest {
                protocol: TunnelProtocol::Http,
                port: 4000,
                subdomain: Some("demo".to_string()),
            })
            .await
            .expect_err("duplicate subdomain should fail");

        assert_eq!(error, CreateTunnelError::SubdomainUnavailable);
    }

    #[tokio::test]
    async fn rejects_registration_with_mismatched_local_addr() {
        let manager = TunnelManager::new("openrok.test");
        let created = manager
            .create_tunnel(CreateTunnelRequest {
                protocol: TunnelProtocol::Http,
                port: 3000,
                subdomain: Some("demo".to_string()),
            })
            .await
            .expect("tunnel should create");
        let (sender, _receiver) = mpsc::unbounded_channel();

        let error = manager
            .register_client(
                created.tunnel_id,
                "http://127.0.0.1:9999".to_string(),
                created.registration_token,
                sender,
            )
            .await
            .expect_err("registration should fail");

        assert!(
            error
                .to_string()
                .contains("local address does not match reserved tunnel")
        );
    }

    #[tokio::test]
    async fn dispatches_request_and_resolves_response() {
        let manager = TunnelManager::new("openrok.test");
        let created = manager
            .create_tunnel(CreateTunnelRequest {
                protocol: TunnelProtocol::Http,
                port: 3000,
                subdomain: Some("demo".to_string()),
            })
            .await
            .expect("tunnel should create");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        manager
            .register_client(
                created.tunnel_id.clone(),
                "http://127.0.0.1:3000".to_string(),
                created.registration_token,
                sender,
            )
            .await
            .expect("registration should succeed");

        let request_id = "req_1".to_string();
        let rx = manager
            .dispatch_request(
                &created.tunnel_id,
                request_id.clone(),
                ControlMessage::ForwardRequest(ForwardRequest {
                    tunnel_id: created.tunnel_id.clone(),
                    request_id: request_id.clone(),
                    method: "GET".to_string(),
                    path: "/".to_string(),
                    query: None,
                    headers: vec![Header {
                        name: "x-test".to_string(),
                        value: "1".to_string(),
                    }],
                    body_base64: encode_body(b""),
                }),
            )
            .await
            .expect("request should dispatch");

        assert!(matches!(
            receiver.recv().await,
            Some(ControlMessage::ForwardRequest(_))
        ));

        assert!(
            manager
                .resolve_response(ForwardResponse {
                    request_id,
                    status: 200,
                    headers: vec![],
                    body_base64: encode_body(b"ok"),
                })
                .await
        );

        let response = rx.await.expect("response should resolve");
        assert_eq!(response.status, 200);
    }
}
