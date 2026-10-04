//! 主机到 mixumd 的 WSS 传输层。

use futures_util::{SinkExt, StreamExt};
use mixum_protocol::envelope::{Envelope, Error as EnvelopeError};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

const CLIENT_NAME: &str = "mixumctl/0.1";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("连接地址无效: {0}")]
    Url(#[from] url::ParseError),
    #[error("WebSocket: {0}")]
    WebSocket(String),
    #[error("协议: {0}")]
    Protocol(String),
    #[error("服务器错误 {code}: {message}")]
    Remote { code: String, message: String },
    #[error("请求超时: {0}")]
    Timeout(String),
    #[error("配置: {0}")]
    Config(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct ConnectionOptions {
    pub addr: String,
    pub token: String,
    pub fingerprint: Option<String>,
    pub known_hosts: PathBuf,
    pub device: String,
}

#[derive(Debug, Clone)]
pub struct BinaryFrame(pub Vec<u8>);

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Envelope>>>>;

#[derive(Clone)]
pub struct Client {
    tx: mpsc::Sender<Message>,
    pending: Pending,
    events: Arc<tokio::sync::Mutex<mpsc::Receiver<Envelope>>>,
    frames: Arc<tokio::sync::Mutex<mpsc::Receiver<BinaryFrame>>>,
    token: String,
    pub fingerprint: String,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct PinVerifier {
    expected: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
    supported_algorithms: WebPkiSupportedAlgorithms,
}

impl PinVerifier {
    fn format(der: &[u8]) -> String {
        Sha256::digest(der)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        let got = Self::format(end_entity.as_ref());
        *self.observed.lock().expect("指纹锁未中毒") = Some(got.clone());
        if let Some(want) = &self.expected {
            if normalize_fingerprint(want) != normalize_fingerprint(&got) {
                return Err(RustlsError::General(format!(
                    "证书指纹不匹配: got {got} want {want}"
                )));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(_message, _cert, _dss, &self.supported_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(_message, _cert, _dss, &self.supported_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn ws_addr(addr: &str) -> Result<String, Error> {
    let raw = if addr.contains("://") {
        addr.to_owned()
    } else {
        format!("wss://{addr}")
    };
    let mut url = url::Url::parse(&raw)?;
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/ws");
    }
    Ok(url.to_string())
}

pub fn known_hosts_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mixum/known-hosts")
}

fn load_known(path: &Path) -> Result<HashMap<String, String>, Error> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(Error::from)
}

pub fn save_known(path: &Path, addr: &str, fingerprint: &str) -> Result<(), Error> {
    let mut hosts = load_known(path)?;
    hosts.insert(addr.to_owned(), fingerprint.to_owned());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(&hosts)?;
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Client {
    pub async fn connect(options: ConnectionOptions) -> Result<Self, Error> {
        let address = ws_addr(&options.addr)?;
        let known = load_known(&options.known_hosts)?;
        let expected = options
            .fingerprint
            .clone()
            .or_else(|| known.get(&options.addr).cloned());
        let first_connection = expected.is_none();
        let observed = Arc::new(Mutex::new(None));
        let provider = rustls::crypto::ring::default_provider();
        let verifier = PinVerifier {
            expected,
            observed: Arc::clone(&observed),
            supported_algorithms: provider.signature_verification_algorithms,
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| Error::Config(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let connector = Connector::Rustls(Arc::new(config));
        let mut request = address
            .into_client_request()
            .map_err(|e| Error::WebSocket(e.to_string()))?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static("mixum.v1"),
        );
        let (stream, _) = connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;
        let fingerprint = observed
            .lock()
            .expect("指纹锁未中毒")
            .clone()
            .ok_or_else(|| Error::Config("TLS 未返回服务器证书".into()))?;
        if first_connection {
            eprintln!("首次连接 {}，服务器证书指纹：{}", options.addr, fingerprint);
        }
        if !known.contains_key(&options.addr) || options.fingerprint.is_some() {
            save_known(&options.known_hosts, &options.addr, &fingerprint)?;
        }
        let (mut write, mut read) = stream.split();
        let (tx, mut outgoing) = mpsc::channel::<Message>(64);
        let (event_tx, event_rx) = mpsc::channel(128);
        let (frame_tx, frame_rx) = mpsc::channel(128);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(message) = read.next().await {
                match message {
                    Ok(Message::Text(text)) => match Envelope::decode(text.as_bytes()) {
                        Ok(env) if !env.reply_to.is_empty() => {
                            let sender = reader_pending
                                .lock()
                                .expect("pending 锁未中毒")
                                .remove(&env.reply_to);
                            if let Some(sender) = sender {
                                let _ = sender.send(env);
                            } else {
                                let _ = event_tx.send(env).await;
                            }
                        }
                        Ok(env) => {
                            let _ = event_tx.send(env).await;
                        }
                        Err(_) => {}
                    },
                    Ok(Message::Binary(data)) => {
                        let _ = frame_tx.send(BinaryFrame(data.to_vec())).await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        });
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if write.send(message).await.is_err() {
                    break;
                }
            }
        });
        let client = Client {
            tx,
            pending,
            events: Arc::new(tokio::sync::Mutex::new(event_rx)),
            frames: Arc::new(tokio::sync::Mutex::new(frame_rx)),
            token: options.token.clone(),
            fingerprint,
        };
        let hello = client
            .call(
                mixum_protocol::ops::op::HELLO,
                serde_json::json!({
                    "token": options.token,
                    "client": CLIENT_NAME,
                    "device": options.device,
                    "versions": ["1"]
                }),
            )
            .await?;
        if hello.op.ends_with(".err") {
            return Err(remote_error(hello));
        }
        Ok(client)
    }

    pub async fn call(&self, op: &str, payload: serde_json::Value) -> Result<Envelope, Error> {
        let request = Envelope::new(op, Some(payload));
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending 锁未中毒")
            .insert(request.id.clone(), sender);
        let raw = request.to_json();
        if self.tx.send(Message::Text(raw.into())).await.is_err() {
            self.pending
                .lock()
                .expect("pending 锁未中毒")
                .remove(&request.id);
            return Err(Error::WebSocket("发送失败，连接已关闭".into()));
        }
        let reply = tokio::time::timeout(Duration::from_secs(180), receiver)
            .await
            .map_err(|_| Error::Timeout(op.to_owned()))?
            .map_err(|_| Error::WebSocket("连接在应答前关闭".into()))?;
        if reply.op.ends_with(".err") {
            return Err(remote_error(reply));
        }
        Ok(reply)
    }

    pub async fn next_event(&self) -> Option<Envelope> {
        self.events.lock().await.recv().await
    }

    pub async fn next_frame(&self) -> Option<BinaryFrame> {
        self.frames.lock().await.recv().await
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

fn remote_error(env: Envelope) -> Error {
    let payload = env.payload.unwrap_or_default();
    Error::Remote {
        code: payload["code"].as_str().unwrap_or("internal").to_owned(),
        message: payload["message"].as_str().unwrap_or("远端错误").to_owned(),
    }
}

impl From<EnvelopeError> for Error {
    fn from(value: EnvelopeError) -> Self {
        Error::Protocol(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_colon_separated_sha256() {
        assert_eq!(PinVerifier::format(b"abc").len(), 95);
        assert_eq!(normalize_fingerprint("AA:bb"), "aabb");
    }

    #[test]
    fn known_hosts_round_trip_uses_json_and_normalized_pin() {
        let dir =
            std::env::temp_dir().join(format!("mixum-ctl-{}", mixum_protocol::envelope::new_id()));
        let path = dir.join("known-hosts");
        save_known(&path, "wss://example.test:7601", "AA:BB").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("wss://example.test:7601"));
        assert!(content.contains("AA:BB"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
