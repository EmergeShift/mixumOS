//! 能力令牌验证和服务端身份。

use mixum_protocol::ops::{Cap, ErrCode};
use mixum_protocol::token::{Claims, Error as TokenError, Signer, Verifier};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

/// 持久化的服务端身份：令牌签名种子、自签 TLS 证书和指纹。
pub struct Identity {
    pub signer: Signer,
    pub tls: Arc<rustls::ServerConfig>,
    pub fingerprint: String,
}

/// 安装进程级 rustls CryptoProvider。
/// workspace 的 rustls 关掉 default-features 只启用 ring,而 rcgen 可能带入
/// aws-lc-rs:两个都在时 rustls 无法自动选择,这里显式钉住 ring。
/// install_default 全进程幂等,重复调用返回 Err,忽略即可。
pub fn ensure_crypto_provider() {
    mixum_protocol::token::ensure_crypto_provider();
}

pub fn load_or_create_identity(
    data_dir: impl AsRef<Path>,
    hosts: &[String],
) -> Result<Identity, Box<dyn std::error::Error + Send + Sync>> {
    let data_dir = data_dir.as_ref();
    std::fs::create_dir_all(data_dir)?;
    let seed_path = data_dir.join("identity.key");
    let seed = match std::fs::read(&seed_path) {
        Ok(seed) if seed.len() == 32 => seed,
        _ => {
            let mut seed = [0u8; 32];
            use rand::RngCore;
            rand::thread_rng().fill_bytes(&mut seed);
            std::fs::write(&seed_path, seed)?;
            seed.to_vec()
        }
    };
    let cert_path = data_dir.join("tls-cert.pem");
    let key_path = data_dir.join("tls-key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let names: Vec<String> = hosts.iter().filter(|h| !h.is_empty()).cloned().collect();
        let cert = rcgen::generate_simple_self_signed(names)?;
        std::fs::write(&cert_path, cert.cert.pem())?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())?;
    }
    ensure_crypto_provider();
    let cert_bytes = std::fs::read(&cert_path)?;
    let key_bytes = std::fs::read(&key_path)?;
    let mut cert_reader = &cert_bytes[..];
    let mut certs = rustls_pemfile::certs(&mut cert_reader);
    let cert = certs.next().ok_or("TLS 证书 PEM 为空")??;
    let mut key_reader = &key_bytes[..];
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut key_reader);
    let key = keys.next().ok_or("TLS 私钥 PEM 为空")??;
    let tls = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key),
        )?;
    let digest = Sha256::digest(cert.as_ref());
    let fingerprint = digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    Ok(Identity {
        signer: Signer::from_seed(seed[..32].try_into().expect("seed length")),
        tls: Arc::new(tls),
        fingerprint,
    })
}

#[derive(Clone)]
pub struct Authorizer {
    verifier: Arc<Verifier>,
}

impl Authorizer {
    pub fn new(verifier: Verifier) -> Self {
        Self {
            verifier: Arc::new(verifier),
        }
    }

    pub fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        self.verifier.verify(token).map_err(AuthError::Token)
    }

    pub fn require(&self, claims: &Claims, cap: Cap) -> Result<(), AuthError> {
        if mixum_protocol::token::has_cap(claims, cap) {
            Ok(())
        } else {
            Err(AuthError::Forbidden(cap))
        }
    }

    pub fn window_allowed(&self, claims: &Claims, window: u32) -> bool {
        mixum_protocol::token::window_allowed(claims, window)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("令牌验证失败：{0}")]
    Token(#[source] TokenError),
    #[error("缺少能力：{0:?}")]
    Forbidden(Cap),
}

impl AuthError {
    pub fn code(&self) -> ErrCode {
        match self {
            Self::Token(_) => ErrCode::Unauthorized,
            Self::Forbidden(_) => ErrCode::Forbidden,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mixum_protocol::token::{Claims, Signer};
    use tempfile::tempdir;

    #[test]
    fn capability_and_window_checks_match_wire() {
        let signer = Signer::generate();
        let auth = Authorizer::new(Verifier::new(signer.verifying_key()));
        let token = signer
            .mint(Claims {
                sub: "test".into(),
                caps: vec![Cap::DisplayView],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![7],
            })
            .unwrap();
        let claims = auth.verify(&token).unwrap();
        assert!(auth.require(&claims, Cap::DisplayView).is_ok());
        assert!(auth.require(&claims, Cap::DisplayInput).is_err());
        assert!(auth.window_allowed(&claims, 7));
        assert!(!auth.window_allowed(&claims, 8));
    }

    #[test]
    fn identity_persists_signing_key_and_certificate_fingerprint() {
        let dir = tempdir().unwrap();
        let hosts = vec!["localhost".to_owned()];
        let first = load_or_create_identity(dir.path(), &hosts).unwrap();
        let token = first
            .signer
            .mint(Claims {
                sub: "persisted".into(),
                caps: vec![Cap::Admin],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap();
        let fingerprint = first.fingerprint.clone();
        let second = load_or_create_identity(dir.path(), &hosts).unwrap();
        assert_eq!(second.fingerprint, fingerprint);
        assert!(Verifier::new(second.signer.verifying_key())
            .verify(&token)
            .is_ok());
        assert!(dir.path().join("identity.key").exists());
        assert!(dir.path().join("tls-cert.pem").exists());
        assert!(dir.path().join("tls-key.pem").exists());
    }
}

/// 不校验服务端证书的 TLS 客户端配置(gateway relay/同步链路的自签场景)。
pub fn insecure_tls_client() -> rustls::ClientConfig {
    ensure_crypto_provider();
    rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth()
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

/// TOFU/固定指纹的 relay TLS 客户端:首次连接记录证书指纹,之后必须匹配。
/// 签名校验仍不做(自签 TOFU 场景),身份由证书指纹固定保证。
pub fn pinned_tls_client(
    expected: Option<String>,
    pin_path: Option<std::path::PathBuf>,
) -> rustls::ClientConfig {
    let expected = expected.map(|value| normalize_fingerprint(&value));
    crate::auth::ensure_crypto_provider();
    rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            expected: std::sync::Mutex::new(expected),
            pin_path,
        }))
        .with_no_client_auth()
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
}

#[derive(Debug)]
struct PinnedVerifier {
    expected: std::sync::Mutex<Option<String>>,
    pin_path: Option<std::path::PathBuf>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(end_entity.as_ref());
        let fingerprint = digest
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let mut expected = self.expected.lock().expect("pin lock");
        match expected.as_deref() {
            Some(want) if want == fingerprint => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            Some(want) => Err(rustls::Error::General(format!(
                "gateway 证书指纹不匹配: got {fingerprint} want {want}"
            ))),
            None => {
                *expected = Some(fingerprint.clone());
                if let Some(path) = &self.pin_path {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    #[cfg(unix)]
                    {
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(path)
                        {
                            let _ = file.write_all(fingerprint.as_bytes());
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = std::fs::write(path, &fingerprint);
                    }
                }
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
