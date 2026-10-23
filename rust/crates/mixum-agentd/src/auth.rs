//! 能力令牌验证和服务端身份。

use mixum_protocol::ops::{Cap, ErrCode};
use mixum_protocol::token::{Claims, Error as TokenError, Signer, Verifier};
use rustls::crypto::{
    verify_tls12_signature as verify_tls12, verify_tls13_signature as verify_tls13,
    WebPkiSupportedAlgorithms,
};
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o600));
            }
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
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
    use rustls::client::danger::ServerCertVerifier;
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

    #[test]
    fn pin_file_is_loaded_and_normalized() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gateway-fingerprint");
        std::fs::write(&path, "aa:bb:cc\n").unwrap();
        assert_eq!(load_pinned(Some(&path)).as_deref(), Some("AABBCC"));
        assert_eq!(normalize_fingerprint("AA:bb:CC"), "AABBCC");
    }

    fn cert_der(name: &str) -> (rustls::pki_types::CertificateDer<'static>, String) {
        let cert = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
        let der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let fingerprint = fingerprint_of(der.as_ref());
        (der, fingerprint)
    }

    #[test]
    fn pinned_verifier_accepts_only_matching_certificate() {
        ensure_crypto_provider();
        let (cert, fingerprint) = cert_der("gateway.local");
        let (other, _) = cert_der("other.local");
        let verifier = PinnedVerifier {
            expected: std::sync::Mutex::new(Some(fingerprint)),
            pin_path: None,
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        };
        let name = rustls::pki_types::ServerName::try_from("gateway.local").unwrap();
        let now = rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(0));
        assert!(verifier
            .verify_server_cert(&cert, &[], &name, &[], now)
            .is_ok());
        assert!(verifier
            .verify_server_cert(&other, &[], &name, &[], now)
            .is_err());
    }

    #[test]
    fn first_tofu_writes_pin_with_0600() {
        ensure_crypto_provider();
        let dir = tempdir().unwrap();
        let path = dir.path().join("gateway-fingerprint");
        let (cert, fingerprint) = cert_der("gateway.local");
        let verifier = PinnedVerifier {
            expected: std::sync::Mutex::new(None),
            pin_path: Some(path.clone()),
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        };
        let name = rustls::pki_types::ServerName::try_from("gateway.local").unwrap();
        let now = rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(0));
        verifier
            .verify_server_cert(&cert, &[], &name, &[], now)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), fingerprint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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

/// TOFU/固定指纹的 relay TLS 客户端:首次连接记录证书指纹,之后必须匹配；
/// 仍按 rustls 的 CertificateVerify 校验服务端握手签名。
pub fn pinned_tls_client(
    expected: Option<String>,
    pin_path: Option<std::path::PathBuf>,
) -> rustls::ClientConfig {
    // 显式指纹优先；否则读上次 TOFU 落盘的 pin；都没有才首次 TOFU。
    let expected = expected
        .map(|value| normalize_fingerprint(&value))
        .or_else(|| load_pinned(pin_path.as_deref()));
    crate::auth::ensure_crypto_provider();
    let provider = rustls::crypto::ring::default_provider();
    let algorithms = provider.signature_verification_algorithms;
    rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS13 由 ring provider 支持")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            expected: std::sync::Mutex::new(expected),
            pin_path,
            algorithms,
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

fn load_pinned(path: Option<&std::path::Path>) -> Option<String> {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .map(|value| normalize_fingerprint(&value))
        .filter(|value| !value.is_empty())
}

fn fingerprint_of(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    digest.iter().map(|byte| format!("{byte:02X}")).collect()
}

#[derive(Debug)]
struct PinnedVerifier {
    expected: std::sync::Mutex<Option<String>>,
    pin_path: Option<std::path::PathBuf>,
    algorithms: WebPkiSupportedAlgorithms,
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
        let fingerprint = fingerprint_of(end_entity.as_ref());
        let mut expected = self.expected.lock().expect("pin lock");
        match expected.as_deref() {
            Some(want) if want == fingerprint => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
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
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls12(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls13(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
