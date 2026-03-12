use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelProtocol {
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTunnelRequest {
    pub protocol: TunnelProtocol,
    pub port: u16,
    pub subdomain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelCreated {
    pub tunnel_id: String,
    pub url: String,
    pub local_addr: String,
    pub registration_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterClient {
    pub tunnel_id: String,
    pub local_addr: String,
    pub registration_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistered {
    pub tunnel_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub tunnel_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRequest {
    pub tunnel_id: String,
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<Header>,
    pub body_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardResponse {
    pub request_id: String,
    pub status: u16,
    pub headers: Vec<Header>,
    pub body_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    CreateTunnel(CreateTunnelRequest),
    TunnelCreated(TunnelCreated),
    RegisterClient(RegisterClient),
    ClientRegistered(ClientRegistered),
    Heartbeat(Heartbeat),
    ForwardRequest(ForwardRequest),
    ForwardResponse(ForwardResponse),
}

pub fn encode_body(body: &[u8]) -> String {
    STANDARD.encode(body)
}

pub fn decode_body(body_base64: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(body_base64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_create_tunnel_message() {
        let message = ControlMessage::CreateTunnel(CreateTunnelRequest {
            protocol: TunnelProtocol::Http,
            port: 3000,
            subdomain: Some("demo".to_string()),
        });

        let json = serde_json::to_string(&message).expect("message should serialize");

        assert!(json.contains("\"type\":\"create_tunnel\""));
        assert!(json.contains("\"protocol\":\"http\""));
        assert!(json.contains("\"port\":3000"));
    }

    #[test]
    fn deserializes_tunnel_created_message() {
        let json = r#"{"type":"tunnel_created","tunnel_id":"tnl_demo","url":"https://demo.openrok.test","local_addr":"http://127.0.0.1:3000","registration_token":"tok_demo"}"#;

        let message: ControlMessage =
            serde_json::from_str(json).expect("message should deserialize");

        assert_eq!(
            message,
            ControlMessage::TunnelCreated(TunnelCreated {
                tunnel_id: "tnl_demo".to_string(),
                url: "https://demo.openrok.test".to_string(),
                local_addr: "http://127.0.0.1:3000".to_string(),
                registration_token: "tok_demo".to_string(),
            })
        );
    }

    #[test]
    fn serializes_register_client_message() {
        let message = ControlMessage::RegisterClient(RegisterClient {
            tunnel_id: "tnl_demo".to_string(),
            local_addr: "http://127.0.0.1:3000".to_string(),
            registration_token: "tok_demo".to_string(),
        });

        let json = serde_json::to_string(&message).expect("message should serialize");

        assert!(json.contains("\"type\":\"register_client\""));
        assert!(json.contains("\"tunnel_id\":\"tnl_demo\""));
    }

    #[test]
    fn round_trips_forward_response() {
        let message = ControlMessage::ForwardResponse(ForwardResponse {
            request_id: "req_123".to_string(),
            status: 200,
            headers: vec![Header {
                name: "content-type".to_string(),
                value: "text/plain".to_string(),
            }],
            body_base64: encode_body(b"ok"),
        });

        let json = serde_json::to_string(&message).expect("message should serialize");
        let decoded: ControlMessage =
            serde_json::from_str(&json).expect("message should deserialize");

        assert_eq!(decoded, message);
    }

    #[test]
    fn round_trips_binary_body() {
        let body = vec![0_u8, 159, 255, 10];
        let encoded = encode_body(&body);
        let decoded = decode_body(&encoded).expect("body should decode");

        assert_eq!(decoded, body);
    }
}
