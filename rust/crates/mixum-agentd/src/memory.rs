//! MAP-Memory:节点本地记忆存储(builtin provider)。
//!
//! 规范见 `Mixum-Agent-Protocol/site/src/content/docs/spec/memory.md`:
//! provider/record 模型、`mem.*` 八个 op、`map-json` 导入导出。
//! 存储位于 `dataDir/memory`,`opaque` 与未知字段原样保留(无损往返)。
//! v1 只实现 `builtin` provider;搜索为子串匹配(规范允许实现自定义排序)。

use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROVIDER_BUILTIN: &str = "builtin";
pub const FORMAT_MAP_JSON: &str = "map-json";

const KINDS: &[&str] = &["fact", "preference", "procedure", "episodic", "custom"];
const SCOPES: &[&str] = &["global", "project", "task"];
const CAPABILITIES: &[&str] = &["read", "write", "search", "import", "export"];

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("记录不合法:{0}")]
    BadRequest(String),
    #[error("记忆不存在:{0}")]
    NotFound(String),
    #[error("存储失败:{0}")]
    Internal(String),
}

impl MemoryError {
    pub fn code(&self) -> mixum_protocol::ops::ErrCode {
        match self {
            Self::BadRequest(_) => mixum_protocol::ops::ErrCode::BadRequest,
            Self::NotFound(_) => mixum_protocol::ops::ErrCode::NotFound,
            Self::Internal(_) => mixum_protocol::ops::ErrCode::Internal,
        }
    }
}

pub struct MemoryStore {
    dir: PathBuf,
    records: Mutex<Vec<Value>>,
}

impl MemoryStore {
    /// 打开(或创建)节点本地记忆库。文件损坏时从空库开始并告警,不阻断启动。
    pub fn open(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let records = match std::fs::read(dir.join("records.json")) {
            Ok(bytes) if !bytes.is_empty() => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => value
                    .get("records")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                Err(err) => {
                    eprintln!("记忆库解析失败,按空库启动: {err}");
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };
        Self {
            dir,
            records: Mutex::new(records),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn persist(&self, records: &[Value]) -> Result<(), MemoryError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| MemoryError::Internal(e.to_string()))?;
        let path = self.dir.join("records.json");
        let tmp = self.dir.join("records.json.tmp");
        let body = serde_json::to_vec(&json!({ "version": 1, "records": records }))
            .map_err(|e| MemoryError::Internal(e.to_string()))?;
        std::fs::write(&tmp, body).map_err(|e| MemoryError::Internal(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| MemoryError::Internal(e.to_string()))?;
        Ok(())
    }

    pub fn providers(&self) -> Value {
        json!({
            "providers": [{
                "id": PROVIDER_BUILTIN,
                "kind": "builtin",
                "status": "ready",
                "capabilities": CAPABILITIES,
            }]
        })
    }

    pub fn get(&self, id: &str) -> Result<Value, MemoryError> {
        let records = self.records.lock().expect("记忆库锁");
        records
            .iter()
            .find(|record| record.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
            .ok_or_else(|| MemoryError::NotFound(id.to_owned()))
    }

    /// `mem.put`:有 id 则更新(保留 createdAt),无 id 则插入并分配 id。
    pub fn put(&self, mut record: Value) -> Result<Value, MemoryError> {
        let mut guard = self.records.lock().expect("记忆库锁");
        normalize_record(&mut record, true)?;
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(existing) = guard
            .iter_mut()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            // 更新:createdAt 以首次写入为准,未知字段由新记录覆盖。
            if let Some(created) = existing.get("createdAt").cloned() {
                record["createdAt"] = created;
            }
            *existing = record.clone();
        } else {
            guard.push(record.clone());
        }
        self.persist(&guard)?;
        Ok(record)
    }

    pub fn delete(&self, id: &str) -> Result<(), MemoryError> {
        let mut guard = self.records.lock().expect("记忆库锁");
        let before = guard.len();
        guard.retain(|record| record.get("id").and_then(Value::as_str) != Some(id));
        if guard.len() == before {
            return Err(MemoryError::NotFound(id.to_owned()));
        }
        self.persist(&guard)?;
        Ok(())
    }

    pub fn list(
        &self,
        provider: Option<&str>,
        kind: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Value {
        let mut records: Vec<Value> = self
            .records
            .lock()
            .expect("记忆库锁")
            .iter()
            .filter(|record| {
                provider.is_none_or(|p| record.get("provider").and_then(Value::as_str) == Some(p))
                    && kind.is_none_or(|k| record.get("kind").and_then(Value::as_str) == Some(k))
            })
            .cloned()
            .collect();
        // 最新写入优先;id 作为同毫秒稳定排序键。
        records.sort_by(|a, b| {
            let key = |record: &Value| {
                (
                    record.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
                    record
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                )
            };
            key(b).cmp(&key(a))
        });
        let start = cursor
            .and_then(|cursor| {
                records
                    .iter()
                    .position(|record| record.get("id").and_then(Value::as_str) == Some(cursor))
                    .map(|index| index + 1)
            })
            .unwrap_or(0);
        let limit = limit.unwrap_or(50).clamp(1, 200);
        let page: Vec<Value> = records
            .iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect();
        let next_cursor = if start + page.len() < records.len() {
            page.last()
                .and_then(|record| record.get("id").and_then(Value::as_str))
                .map(|id| json!(id))
        } else {
            None
        };
        match next_cursor {
            Some(cursor) => json!({ "records": page, "nextCursor": cursor }),
            None => json!({ "records": page }),
        }
    }

    pub fn search(
        &self,
        query: &str,
        kinds: Option<&[String]>,
        providers: Option<&[String]>,
        scope: Option<&Value>,
        limit: Option<usize>,
    ) -> Value {
        let query_lower = query.to_lowercase();
        let mut hits: Vec<(Value, f64)> = self
            .records
            .lock()
            .expect("记忆库锁")
            .iter()
            .filter(|record| {
                kinds.is_none_or(|kinds| {
                    record
                        .get("kind")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kinds.iter().any(|want| want == kind))
                }) && providers.is_none_or(|providers| {
                    record.get("provider").and_then(Value::as_str).is_some_and(|provider| {
                        providers.iter().any(|want| want == provider)
                    })
                }) && scope.is_none_or(|scope| record.get("scope") == Some(scope))
            })
            .filter_map(|record| {
                let text = record
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text_lower = text.to_lowercase();
                let tag_hit = record
                    .get("tags")
                    .and_then(Value::as_array)
                    .is_some_and(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_str()
                                .is_some_and(|tag| tag.to_lowercase().contains(&query_lower))
                        })
                    });
                let score = if query_lower.is_empty() {
                    0.5
                } else if text_lower == query_lower {
                    1.0
                } else if text_lower.contains(&query_lower) {
                    0.9
                } else if tag_hit {
                    0.8
                } else {
                    return None;
                };
                Some((record.clone(), score))
            })
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1));
        let limit = limit.unwrap_or(20).clamp(1, 200);
        let hits: Vec<Value> = hits
            .into_iter()
            .take(limit)
            .map(|(record, score)| json!({ "record": record, "score": score }))
            .collect();
        json!({ "hits": hits })
    }

    /// `mem.import`:upsert-by-id;无 id 一律插入;校验失败计入 skipped。
    pub fn import(
        &self,
        provider: &str,
        format: &str,
        records: Vec<Value>,
    ) -> Result<Value, MemoryError> {
        if format != FORMAT_MAP_JSON {
            return Err(MemoryError::BadRequest(format!(
                "不支持的导入格式:{format}(v1 仅支持 {FORMAT_MAP_JSON})"
            )));
        }
        let mut guard = self.records.lock().expect("记忆库锁");
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for mut record in records {
            if record.get("provider").is_none() && !provider.is_empty() {
                record["provider"] = json!(provider);
            }
            if normalize_record(&mut record, false).is_err() {
                skipped += 1;
                continue;
            }
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() {
                record["id"] = json!(mixum_protocol::envelope::new_id());
                guard.push(record);
                imported += 1;
                continue;
            }
            if let Some(existing) = guard
                .iter_mut()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
            {
                if let Some(created) = existing.get("createdAt").cloned() {
                    record["createdAt"] = created;
                }
                *existing = record;
            } else {
                guard.push(record);
            }
            imported += 1;
        }
        self.persist(&guard)?;
        Ok(json!({ "imported": imported, "skipped": skipped }))
    }

    pub fn export(
        &self,
        provider: Option<&str>,
        format: &str,
    ) -> Result<Value, MemoryError> {
        if format != FORMAT_MAP_JSON {
            return Err(MemoryError::BadRequest(format!(
                "不支持的导出格式:{format}(v1 仅支持 {FORMAT_MAP_JSON})"
            )));
        }
        let records: Vec<Value> = self
            .records
            .lock()
            .expect("记忆库锁")
            .iter()
            .filter(|record| {
                provider.is_none_or(|p| record.get("provider").and_then(Value::as_str) == Some(p))
            })
            .cloned()
            .collect();
        Ok(json!({ "records": records }))
    }
}

/// 规范化并校验一条 MAP memory record(schema: memory-record.schema.json)。
fn normalize_record(record: &mut Value, assign_id: bool) -> Result<(), MemoryError> {
    let bad = |message: &str| MemoryError::BadRequest(message.to_owned());
    let object = record
        .as_object_mut()
        .ok_or_else(|| bad("record 必须是 JSON 对象"))?;

    let provider = object
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if provider.is_empty() {
        return Err(bad("provider 不能为空"));
    }
    if provider != PROVIDER_BUILTIN {
        return Err(bad(&format!("未知 provider:{provider}")));
    }

    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !KINDS.contains(&kind) {
        return Err(bad(&format!(
            "kind 必须是 {} 之一,收到 {kind}",
            KINDS.join("|")
        )));
    }

    let text = object
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err(bad("content.text 不能为空"));
    }
    if object
        .get("content")
        .and_then(|content| content.get("structured"))
        .is_some_and(|structured| !structured.is_object())
    {
        return Err(bad("content.structured 必须是对象"));
    }

    let level = object
        .get("scope")
        .and_then(|scope| scope.get("level"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !SCOPES.contains(&level) {
        return Err(bad(&format!(
            "scope.level 必须是 {} 之一,收到 {level}",
            SCOPES.join("|")
        )));
    }
    if object
        .get("scope")
        .and_then(|scope| scope.get("ref"))
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        return Err(bad("scope.ref 必须是字符串"));
    }

    if let Some(tags) = object.get("tags") {
        let Some(tags) = tags.as_array() else {
            return Err(bad("tags 必须是字符串数组"));
        };
        let mut seen = HashSet::new();
        for tag in tags {
            let Some(tag) = tag.as_str() else {
                return Err(bad("tags 必须是字符串数组"));
            };
            if !seen.insert(tag.to_owned()) {
                return Err(bad("tags 不得重复"));
            }
        }
    }

    if let Some(confidence) = object.get("confidence") {
        if !confidence
            .as_f64()
            .is_some_and(|value| (0.0..=1.0).contains(&value))
        {
            return Err(bad("confidence 必须在 0..1"));
        }
    }
    if object
        .get("opaque")
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        return Err(bad("opaque 必须是 base64 字符串或 null"));
    }

    if assign_id {
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if id.is_empty() {
            object.insert("id".into(), json!(mixum_protocol::envelope::new_id()));
        } else if id.len() != 26 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(bad("id 必须是 26 字符 ULID"));
        }
    }

    let now = now_ms();
    let created = object
        .get("createdAt")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(now);
    object.insert("createdAt".into(), json!(created));
    object.insert("updatedAt".into(), json!(now));
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn record(text: &str) -> Value {
        json!({
            "provider": "builtin",
            "kind": "preference",
            "content": { "text": text, "structured": { "locale": "zh-CN" } },
            "scope": { "level": "global" },
            "source": { "platform": "chatgpt", "origin": "import" },
            "tags": ["style"],
            "opaque": "AAEC",
        })
    }

    #[test]
    fn put_get_update_search_and_delete() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path());
        let saved = store.put(record("Prefers concise answers")).unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        assert_eq!(id.len(), 26, "记录 id 应为 ULID");
        assert!(saved["createdAt"].as_i64().unwrap() > 0);

        let fetched = store.get(&id).unwrap();
        assert_eq!(fetched["content"]["text"], "Prefers concise answers");
        assert_eq!(fetched["opaque"], "AAEC", "opaque 必须无损保留");

        let hits = store.search("concise", None, None, None, None);
        assert_eq!(hits["hits"][0]["record"]["id"], json!(id));
        assert_eq!(hits["hits"][0]["score"], 0.9);

        let mut updated = fetched.clone();
        updated["content"]["text"] = json!("Prefers very concise answers");
        let updated = store.put(updated).unwrap();
        assert_eq!(updated["createdAt"], saved["createdAt"], "更新保留 createdAt");
        assert!(updated["updatedAt"].as_i64() >= saved["updatedAt"].as_i64());

        store.delete(&id).unwrap();
        assert!(matches!(store.get(&id), Err(MemoryError::NotFound(_))));
    }

    #[test]
    fn import_export_skips_invalid_records() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path());
        let imported = store
            .import(
                PROVIDER_BUILTIN,
                FORMAT_MAP_JSON,
                vec![record("fact one"), json!({ "kind": "fact" })],
            )
            .unwrap();
        assert_eq!(imported["imported"], 1);
        assert_eq!(imported["skipped"], 1);
        let exported = store.export(None, FORMAT_MAP_JSON).unwrap();
        assert_eq!(exported["records"].as_array().unwrap().len(), 1);
        assert!(store.import(PROVIDER_BUILTIN, "chatgpt-export", vec![]).is_err());
    }

    #[test]
    fn records_survive_reopen() {
        let dir = tempdir().unwrap();
        let id;
        {
            let store = MemoryStore::open(dir.path());
            id = store.put(record("persisted")).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
        }
        let reopened = MemoryStore::open(dir.path());
        assert_eq!(reopened.get(&id).unwrap()["content"]["text"], "persisted");
    }

    #[test]
    fn list_paginates_with_cursor() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path());
        for index in 0..3 {
            store.put(record(&format!("record {index}"))).unwrap();
        }
        let first = store.list(None, None, None, Some(2));
        let page = first["records"].as_array().unwrap();
        assert_eq!(page.len(), 2);
        let cursor = first["nextCursor"].as_str().unwrap().to_owned();
        let second = store.list(None, None, Some(&cursor), Some(2));
        assert_eq!(second["records"].as_array().unwrap().len(), 1);
        assert!(second.get("nextCursor").is_none());
    }
}
