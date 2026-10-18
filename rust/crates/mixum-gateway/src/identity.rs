//! gateway 身份:ed25519 种子 + 自签 TLS 证书持久化(对齐 mixumd auth.rs 语义)。
//! TOFU:证书 10 年,指纹落 stdout 供客户端核对;节点经 relay 的 insecure
//! 通道接入(M1/M2 限定,公网部署换受信证书)。

use mixum_protocol::token::Signer;
use sha2::Digest;
use std::path::Path;
use std::sync::Arc;

pub struct Identity {
    pub signer: Signer,
    pub tls: Arc<rustls::ServerConfig>,
    pub fingerprint: String,
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
            use rand::RngCore;
            let mut seed = [0u8; 32];
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
    mixum_protocol::token::ensure_crypto_provider();    let cert_bytes = std::fs::read(&cert_path)?;
    let key_bytes = std::fs::read(&key_path)?;
    let mut cert_reader = &cert_bytes[..];
    let cert = rustls_pemfile::certs(&mut cert_reader)
        .next()
        .ok_or("TLS 证书 PEM 为空")??;
    let mut key_reader = &key_bytes[..];
    let key = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
        .next()
        .ok_or("TLS 私钥 PEM 为空")??;
    let tls = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(key))?;
    let digest = sha2::Sha256::digest(cert.as_ref());
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
