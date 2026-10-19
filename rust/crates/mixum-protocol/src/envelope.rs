//! envelope:所有控制消息的统一外壳(文本帧,JSON)。

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::PROTOCOL_VERSION;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("消息不是合法 JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("协议版本不支持: got {0} want {1}")]
    Version(u32, u32),
    #[error("缺少 id 或 op")]
    MissingField,
}

/// Envelope 是所有控制消息的统一外壳。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    pub op: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seq: i64,
    #[serde(default, rename = "replyTo", skip_serializing_if = "String::is_empty")]
    pub reply_to: String,
    /// 可选:gateway 路由目标节点名(直连 mixumd 时不存在,忽略即可)。
    #[serde(default, rename = "node", skip_serializing_if = "String::is_empty")]
    pub node: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "payload")]
    pub payload: Option<Value>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// Canonical ULID:48 位毫秒时间戳 + 80 位随机数,26 字符 Crockford base32。
/// MAP envelope.md §Identifiers 要求所有 envelope/任务/快照/记忆 id 使用 ULID;
/// ULID 按时间可排序,列表与日志天然带因果顺序。
pub fn new_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ts.to_be_bytes()[2..]);
    rand::thread_rng().fill_bytes(&mut b[6..]);
    ulid_from_bytes(&b)
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 16 字节(前 6 字节时间戳)编码为 26 字符 ULID。
pub fn ulid_from_bytes(bytes: &[u8; 16]) -> String {
    let mut value = u128::from_be_bytes(*bytes);
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value & 0x1f) as usize];
        value >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("Crockford alphabet 是 ASCII")
}

/// 当前 unix 毫秒(信封 ts 与事件时间戳共用)。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Envelope {
    /// 构造一条外发消息,自动填充版本、id 与时间戳。
    pub fn new(op: &str, payload: Option<Value>) -> Self {
        Envelope {
            v: PROTOCOL_VERSION,
            id: new_id(),
            op: op.to_string(),
            seq: 0,
            reply_to: String::new(),
            node: String::new(),
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            payload,
        }
    }

    /// 解析并校验一条入站消息。
    pub fn decode(data: &[u8]) -> Result<Envelope, Error> {
        let e: Envelope = serde_json::from_slice(data)?;
        if e.v != PROTOCOL_VERSION {
            return Err(Error::Version(e.v, PROTOCOL_VERSION));
        }
        if e.id.is_empty() || e.op.is_empty() {
            return Err(Error::MissingField);
        }
        Ok(e)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("envelope 序列化不会失败")
    }

    /// 给推送方向的 envelope 打上会话内单调序号(session.resume 依据)。
    pub fn with_seq(mut self, seq: i64) -> Self {
        self.seq = seq;
        self
    }

    /// 以 ok 形式回复,负载为 resp。
    pub fn reply(&self, payload: Option<Value>) -> Envelope {
        let mut r = Envelope::new(&format!("{}.ok", self.op), payload);
        r.reply_to = self.id.clone();
        r
    }

    /// 回复一条带错误码的响应(op.err)。
    pub fn error_reply(&self, code: crate::ops::ErrCode, message: &str) -> Envelope {
        let mut r = Envelope::new(
            &format!("{}.err", self.op),
            Some(serde_json::json!({ "code": code, "message": message })),
        );
        r.reply_to = self.id.clone();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_unique_and_versioned() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let id = new_id();
            assert!(!seen.contains(&id), "id 重复: {id}");
            seen.insert(id);
        }
    }

    #[test]
    fn id_is_canonical_ulid() {
        let id = new_id();
        assert_eq!(id.len(), 26, "ULID 必须是 26 字符: {id}");
        assert!(
            id.chars().all(|c| CROCKFORD.contains(&(c as u8))),
            "ULID 字符集必须是 Crockford base32: {id}"
        );
        // 时间戳前缀使 ULID 可按字典序排序:同一毫秒内随机位仍可能乱序,
        // 故只验证已知字节向量的编码,不做跨毫秒断言。
        let bytes = [
            0x01, 0x72, 0x8f, 0x3a, 0x4b, 0x5c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(ulid_from_bytes(&bytes).len(), 26);
        assert!(ulid_from_bytes(&bytes).starts_with('0'));
    }

    #[test]
    fn round_trip() {
        let e = Envelope::new(
            "task.submit",
            Some(serde_json::json!({ "instruction": "打开 b 站搜 mixumOS" })),
        );
        let raw = e.to_json();
        let got = Envelope::decode(raw.as_bytes()).expect("往返应成功");
        assert_eq!(got.op, "task.submit");
        assert_eq!(got.id, e.id);
        assert_eq!(got.payload.unwrap()["instruction"], "打开 b 站搜 mixumOS");
    }

    #[test]
    fn rejects_bad_version() {
        let mut e = Envelope::new("sys.ping", None);
        e.v = 99;
        let raw = serde_json::to_string(&e).unwrap();
        assert!(Envelope::decode(raw.as_bytes()).is_err(), "应拒绝未知版本");
    }

    #[test]
    fn reply_carries_reply_to() {
        let e = Envelope::new("snap.tree", None);
        let r = e.reply(Some(serde_json::json!({ "head": "n1" })));
        assert_eq!(r.reply_to, e.id);
        assert_eq!(r.op, "snap.tree.ok");
    }

    #[test]
    fn error_reply_shape() {
        let e = Envelope::new("display.input", None);
        let r = e.error_reply(crate::ops::ErrCode::Forbidden, "缺少能力");
        assert_eq!(r.op, "display.input.err");
        let p = r.payload.unwrap();
        assert_eq!(p["code"], "forbidden");
    }
}

#[cfg(test)]
mod wire_compat {
    use super::*;

    /// 线上字段名必须与 Go 参考实现一致(驼峰),否则跨实现互不认。
    #[test]
    fn field_names_match_reference() {
        let mut e = Envelope::new("task.submit", Some(serde_json::json!({"a": 1})));
        e.reply_to = "req-1".into();
        let val: serde_json::Value = serde_json::from_str(&e.to_json()).unwrap();
        assert!(val.get("replyTo").is_some(), "应为 replyTo");
        assert!(val.get("reply_to").is_none(), "不得出现 reply_to");
        assert!(val.get("payload").is_some(), "字段名应为 payload");
        assert!(val.get("ts").is_some());
        // 无负载时与 Go 的 omitempty 行为一致:字段整体缺席。
        let bare = Envelope::new("sys.ping", None);
        let val2: serde_json::Value = serde_json::from_str(&bare.to_json()).unwrap();
        assert!(val2.get("payload").is_none());
    }
}
