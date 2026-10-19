//! 能力令牌:`map1.<base64url(payload)>.<base64url(sig)>`,ed25519 签名。
//! 语义与已移除的 Go 参考实现一致,兼容向量固化于 tests/cross-protocol:
//! 长期令牌允许跨连接复用(jti 留给吊销列表),过期即拒。
//! 过渡期:旧前缀 `mixum1.` 仍被接受(验证端双读),签发端只发 `map1.`。

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// rustls 加密提供者:钉 ring(aws-lc-rs 与 ring 并存时 rustls 无法自动
/// 选择);install_default 全进程幂等,重复调用返回 Err,忽略即可。
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

use crate::ops::Cap;

/// MAP 规范前缀(token.schema.json 标题为 `map1.`)。
pub const TOKEN_PREFIX: &str = "map1.";
/// 过渡期兼容前缀:已签发的旧令牌继续有效,不再签发。
pub const LEGACY_TOKEN_PREFIX: &str = "mixum1.";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("令牌格式不正确")]
    Malformed,
    #[error("令牌签名无效")]
    BadSignature,
    #[error("令牌已过期")]
    Expired,
    #[error("编解码失败: {0}")]
    Encoding(String),
    #[error("序列化失败: {0}")]
    Serialize(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    /// 主体:设备或用户标识。
    pub sub: String,
    /// 授予的能力集合(最小授予)。
    pub caps: Vec<Cap>,
    /// 签发时间(unix 秒)。
    pub iat: i64,
    /// 过期时间(unix 秒)。
    pub exp: i64,
    /// 唯一 id,预留给吊销列表;v1 不做单次使用检查。
    pub jti: String,
    /// 可选:限定可操作任务(Go 端 JSON tag 为 taskId)。
    #[serde(default, rename = "taskId", skip_serializing_if = "String::is_empty")]
    pub task_id: String,
    /// 可选:限定可观看/接管窗口;缺省全量。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<u32>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 签发端(服务端身份)。
pub struct Signer {
    key: SigningKey,
}

impl Signer {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Signer {
            key: SigningKey::from_bytes(seed),
        }
    }

    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut seed);
        Self::from_seed(&seed)
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// 签发一个令牌。iat/jti/exp 缺省自动填充(exp 默认 1 小时)。
    pub fn mint(&self, mut claims: Claims) -> Result<String, Error> {
        let now = now_unix();
        if claims.jti.is_empty() {
            claims.jti = crate::envelope::new_id();
        }
        if claims.iat == 0 {
            claims.iat = now;
        }
        if claims.exp == 0 {
            claims.exp = claims.iat + 3600;
        }
        let payload = serde_json::to_vec(&claims).map_err(|e| Error::Serialize(e.to_string()))?;
        let sig = self.key.sign(&payload);
        Ok(format!(
            "{}{}.{}",
            TOKEN_PREFIX,
            B64.encode(&payload),
            B64.encode(sig.to_bytes())
        ))
    }
}

/// 验证端(服务端与任何持有公钥的验证方)。
#[derive(Clone)]
pub struct Verifier {
    pub_key: VerifyingKey,
}

impl Verifier {
    pub fn new(pub_key: VerifyingKey) -> Self {
        Verifier { pub_key }
    }

    pub fn verify(&self, raw: &str) -> Result<Claims, Error> {
        let raw = raw.trim();
        let rest = raw
            .strip_prefix(TOKEN_PREFIX)
            .or_else(|| raw.strip_prefix(LEGACY_TOKEN_PREFIX))
            .ok_or(Error::Malformed)?;
        let dot = rest.rfind('.').ok_or(Error::Malformed)?;
        let payload = B64
            .decode(&rest[..dot])
            .map_err(|e| Error::Encoding(e.to_string()))?;
        let sig = B64
            .decode(&rest[dot + 1..])
            .map_err(|e| Error::Encoding(e.to_string()))?;
        let sig = Signature::from_slice(&sig).map_err(|_| Error::Malformed)?;
        if self.pub_key.verify(&payload, &sig).is_err() {
            return Err(Error::BadSignature);
        }
        let claims: Claims =
            serde_json::from_slice(&payload).map_err(|e| Error::Encoding(e.to_string()))?;
        if now_unix() > claims.exp {
            return Err(Error::Expired);
        }
        Ok(claims)
    }
}

/// 拥有 have 是否满足需要 need(admin 覆盖一切)。
pub fn has_cap(claims: &Claims, need: Cap) -> bool {
    Cap::covers(&claims.caps, need)
}

/// claims 是否覆盖某窗口(未限定则全覆盖)。
pub fn window_allowed(claims: &Claims, id: u32) -> bool {
    claims.windows.is_empty() || claims.windows.contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Signer, Verifier) {
        let s = Signer::generate();
        let v = Verifier::new(s.verifying_key());
        (s, v)
    }

    #[test]
    fn mint_verify_round_trip() {
        let (s, v) = pair();
        let tok = s
            .mint(Claims {
                sub: "iphone-15".into(),
                caps: vec![Cap::TaskRun, Cap::DisplayView],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap();
        let c = v.verify(&tok).unwrap();
        assert_eq!(c.sub, "iphone-15");
        assert!(has_cap(&c, Cap::TaskRun));
        assert!(!has_cap(&c, Cap::Admin));
    }

    #[test]
    fn rejects_tampered_and_foreign() {
        let (s, v) = pair();
        let tok = s
            .mint(Claims {
                sub: "x".into(),
                caps: vec![Cap::Admin],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap();
        let bad = format!("{}AAAA", &tok[..tok.len() - 4]);
        assert!(v.verify(&bad).is_err(), "被篡改的令牌应被拒绝");

        let (other, _) = pair();
        let tok2 = other
            .mint(Claims {
                sub: "y".into(),
                caps: vec![],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap();
        assert!(v.verify(&tok2).is_err(), "非本服务签发应被拒绝");
    }

    #[test]
    fn rejects_expired() {
        let (s, v) = pair();
        let tok = s
            .mint(Claims {
                sub: "x".into(),
                caps: vec![],
                iat: now_unix() - 7200,
                exp: now_unix() - 60,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap();
        assert!(matches!(v.verify(&tok), Err(Error::Expired)));
    }

    #[test]
    fn token_reuse_is_allowed() {
        // 长期令牌跨连接复用是设计决策(见 docs/security.md §2)。
        let (s, v) = pair();
        let tok = s
            .mint(Claims {
                sub: "x".into(),
                caps: vec![Cap::SnapSync],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap();
        for _ in 0..3 {
            assert!(v.verify(&tok).is_ok());
        }
    }

    #[test]
    fn window_scoping() {
        let c = Claims {
            sub: "x".into(),
            caps: vec![],
            iat: 0,
            exp: 0,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![1, 3],
        };
        assert!(window_allowed(&c, 3));
        assert!(!window_allowed(&c, 2));
        let all = Claims {
            windows: vec![],
            ..c.clone()
        };
        assert!(window_allowed(&all, 9));
    }

    /// 与 Go 参考实现的交叉验证向量:
    /// 种子固定为 32 字节 0xAB,Go Signer.Mint 的输出必须被本实现接受。
    /// 向量由 genvector 预生成(工具已随 Go 参考实现移除),固化于 tests/cross-protocol/。
    #[test]
    fn cross_verify_with_go_reference_vector() {
        let vector = include_str!("../../../../tests/cross-protocol/token-vector.json");
        let v: serde_json::Value = serde_json::from_str(vector).unwrap();
        let seed: Vec<u8> = v["seed_hex"]
            .as_str()
            .unwrap()
            .chars()
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|c| u8::from_str_radix(&c.iter().collect::<String>(), 16).unwrap())
            .collect();
        let s = Signer::from_seed(seed[..32].try_into().unwrap());
        let verifier = Verifier::new(s.verifying_key());
        let claims = verifier.verify(v["token"].as_str().unwrap()).unwrap();
        assert_eq!(claims.sub, v["expect"]["sub"].as_str().unwrap());
        assert_eq!(
            claims.caps.len(),
            v["expect"]["caps"].as_array().unwrap().len()
        );
    }

    #[test]
    fn mint_emits_map1_prefix_and_accepts_legacy_prefix() {
        let (s, v) = pair();
        let claims = Claims {
            sub: "phase".into(),
            caps: vec![Cap::MemRead, Cap::MemWrite],
            iat: 0,
            exp: 0,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![],
        };
        let map_token = s.mint(claims.clone()).unwrap();
        assert!(map_token.starts_with("map1."), "签发端必须使用 MAP 前缀");
        assert!(v.verify(&map_token).is_ok());

        // 旧镜像签发的 mixum1. 令牌在过渡期继续可用。
        let legacy = format!("mixum1.{}", map_token.strip_prefix("map1.").unwrap());
        let verified = v.verify(&legacy).expect("旧前缀应继续被接受");
        assert!(has_cap(&verified, Cap::MemWrite));
    }
}
