//! 节点注册表抽象(M3)。
//!
//! gateway 的路由核心只依赖 [`NodeRegistry`] 的**元数据**视图;真实的
//! 出站 socket 永远留在持有它的 gateway 进程里(`Gateway::local`)。
//! 这个拆分让注册表可以落 k8s(多副本共享),而连接不被跨进程复制。
//!
//! 记录里的 `owner` 是 gateway 实例标识(单副本为 `local`,k8s 为 Pod 名),
//! `owner_addr` 是该实例的 mesh 地址(M3b 跨副本转发用)。

use crate::OutTx;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

/// 注册表中的一条节点元数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub name: String,
    pub version: String,
    /// 注册时间(unix 毫秒)。
    pub since: i64,
    /// 持有该节点连接的 gateway 实例。
    pub owner: String,
    /// 该实例的 mesh 地址(`ws://host:port/mesh`);单副本为空。
    pub owner_addr: String,
}

/// 节点注册表(元数据)。
///
/// 不变式:
/// - `register` 对在线同名节点返回错误(任何 owner;过期租约可由新连接接管);
/// - `remove` 只在 owner 匹配时生效,避免旧连接关掉新连接;
/// - `get` / `online` 过滤过期租约(k8s 后端按 renewTime 判定)。
#[async_trait]
pub trait NodeRegistry: Send + Sync {
    async fn register(&self, record: NodeRecord) -> Result<(), String>;
    async fn remove(&self, name: &str, owner: &str);
    async fn get(&self, name: &str) -> Option<NodeRecord>;
    /// 在线节点名(排序后返回,便于日志与断言稳定)。
    async fn online(&self) -> Vec<String>;

    /// 节点心跳续约;内存实现无操作。
    async fn renew(&self, _name: &str, _owner: &str, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }

    async fn len(&self) -> usize {
        self.online().await.len()
    }

    async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// 进程内实现:M1/M2 与单副本默认后端。
#[derive(Default)]
pub struct MemoryRegistry {
    nodes: RwLock<HashMap<String, NodeRecord>>,
    /// 租约有效期(毫秒);内存实现只在 renew 时刷新 `since` 之外的活性。
    pub lease_ms: i64,
}

impl MemoryRegistry {
    pub fn new() -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            lease_ms: 0,
        }
    }
}

#[async_trait]
impl NodeRegistry for MemoryRegistry {
    async fn register(&self, record: NodeRecord) -> Result<(), String> {
        let mut nodes = self.nodes.write().await;
        if nodes.contains_key(&record.name) {
            return Err(format!("节点重名在线:{}", record.name));
        }
        nodes.insert(record.name.clone(), record);
        Ok(())
    }

    async fn remove(&self, name: &str, owner: &str) {
        let mut nodes = self.nodes.write().await;
        if nodes.get(name).is_some_and(|record| record.owner == owner) {
            nodes.remove(name);
        }
    }

    async fn get(&self, name: &str) -> Option<NodeRecord> {
        self.nodes.read().await.get(name).cloned()
    }

    async fn online(&self) -> Vec<String> {
        let mut names: Vec<String> = self.nodes.read().await.keys().cloned().collect();
        names.sort();
        names
    }

    async fn renew(&self, name: &str, owner: &str, _now_ms: i64) -> Result<(), String> {
        let nodes = self.nodes.read().await;
        match nodes.get(name) {
            Some(record) if record.owner == owner => Ok(()),
            Some(_) => Err(format!("节点 {name} 不属于本实例")),
            None => Err(format!("节点不在线: {name}")),
        }
    }
}

/// 本进程持有的出站连接表(k8s 后端下只有本 Pod 的条目)。
pub type LocalNodes = RwLock<HashMap<String, OutTx>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, owner: &str) -> NodeRecord {
        NodeRecord {
            name: name.into(),
            version: "0.1.0".into(),
            since: 1,
            owner: owner.into(),
            owner_addr: String::new(),
        }
    }

    #[tokio::test]
    async fn duplicate_registration_is_rejected() {
        let registry = MemoryRegistry::new();
        registry.register(record("a", "gw-1")).await.unwrap();
        assert!(registry.register(record("a", "gw-1")).await.is_err());
        assert!(registry.register(record("a", "gw-2")).await.is_err());
        assert_eq!(registry.online().await, vec!["a".to_owned()]);
    }

    #[tokio::test]
    async fn remove_respects_owner_and_is_idempotent() {
        let registry = MemoryRegistry::new();
        registry.register(record("a", "gw-1")).await.unwrap();
        registry.remove("a", "gw-2").await;
        assert_eq!(registry.online().await, vec!["a".to_owned()]);
        registry.remove("a", "gw-1").await;
        registry.remove("a", "gw-1").await;
        assert!(registry.online().await.is_empty());
    }

    #[tokio::test]
    async fn online_is_sorted_and_renew_touches_owner() {
        let registry = MemoryRegistry::new();
        registry.register(record("b", "gw-1")).await.unwrap();
        registry.register(record("a", "gw-1")).await.unwrap();
        assert_eq!(
            registry.online().await,
            vec!["a".to_owned(), "b".to_owned()]
        );
        registry.renew("a", "gw-1", 42).await.unwrap();
        registry.renew("a", "gw-2", 43).await.unwrap_err();
        assert_eq!(
            registry.get("a").await.unwrap().since,
            1,
            "renew 不改注册时间"
        );
    }
}
