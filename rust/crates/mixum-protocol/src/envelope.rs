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
    #[serde(
        default,
        rename = "replyTo",
        skip_serializing_if = "String::is_empty"
    )]
    pub reply_to: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "payload")]
    pub payload: Option<Value>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// uuidv7 形式 id(48 位毫秒时间戳 + 随机),与 Go 参考实现同格式。
pub fn new_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut b = [0u8; 16];
    b[0] = (ts >> 40) as u8;
    b[1] = (ts >> 32) as u8;
    b[2] = (ts >> 24) as u8;
    b[3] = (ts >> 16) as u8;
    b[4] = (ts >> 8) as u8;
    b[5] = ts as u8;
    rand::thread_rng().fill_bytes(&mut b[6..]);
    b[6] = (b[6] & 0x0f) | 0x70; // version 7
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    format_hex(&b)
}

fn format_hex(b: &[u8; 16]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
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
    fn round_trip() {
        let e = Envelope::new(
            "task.submit",
            Some(serde_json::json!({ "instruction": "打开 b 站搜 mixumOS" })),
        );
        let raw = e.to_json();
        let got = Envelope::decode(raw.as_bytes()).expect("往返应成功");
        assert_eq!(got.op, "task.submit");
        assert_eq!(got.id, e.id);
        assert_eq!(
            got.payload.unwrap()["instruction"],
            "打开 b 站搜 mixumOS"
        );
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
