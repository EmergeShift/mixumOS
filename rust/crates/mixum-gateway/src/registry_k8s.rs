//! k8s 后端:节点注册表落 `coordination.k8s.io/v1` Lease(每个节点一条)。
//!
//! 选择 Lease 而不是新 CRD 的原因:Lease 是 k8s 原生的「谁在线 / 租约」原语,
//! 自带 `holderIdentity`、`renewTime`、`leaseDurationSeconds`,RBAC 也简单
//! (`leases` 的 get/list/watch/create/update/delete/patch)。节点元数据
//! (owner mesh 地址、版本、注册时间)放 annotation。
//!
//! 多副本语义:每个 gateway Pod 用自己的实例名作 `holderIdentity`;注册表
//! 共享后,任何副本都能 `get/online`,但出站 socket 只留在 owner Pod。

use crate::registry::{NodeRecord, NodeRegistry};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use kube::Client;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const LABEL_REGISTRY: &str = "mixumos.io/registry";
const LABEL_REGISTRY_VALUE: &str = "gateway";
const ANN_NODE: &str = "mixumos.io/node";
const ANN_OWNER: &str = "mixumos.io/owner";
const ANN_OWNER_ADDR: &str = "mixumos.io/owner-addr";
const ANN_VERSION: &str = "mixumos.io/version";
const ANN_SINCE: &str = "mixumos.io/since";
/// 节点侧 relay 每 20s 心跳;租约给 3 个心跳窗口,进程崩溃后最多 60s 清理。
pub const LEASE_SECS: i32 = 60;

pub struct K8sRegistry {
    client: Client,
    namespace: String,
}

impl K8sRegistry {
    pub async fn new(namespace: impl Into<String>) -> Result<Self, String> {
        let client = Client::try_default()
            .await
            .map_err(|e| format!("kube client 初始化失败: {e}"))?;
        Ok(Self {
            client,
            namespace: namespace.into(),
        })
    }

    fn api(&self) -> Api<Lease> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn lease_name(node: &str) -> String {
        let mut slug: String = node
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        while slug.contains("--") {
            slug = slug.replace("--", "-");
        }
        let slug = slug.trim_matches('-').to_owned();
        let mut short = if slug.len() > 40 { &slug[..40] } else { &slug };
        if short.is_empty() {
            short = "node";
        }
        let mut hasher = Sha256::new();
        hasher.update(node.to_lowercase().as_bytes());
        let digest = hasher.finalize();
        format!(
            "mixum-node-{short}-{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        )
    }

    fn expired(lease: &Lease, now: DateTime<Utc>) -> bool {
        let Some(spec) = lease.spec.as_ref() else {
            return true;
        };
        let Some(renew) = spec.renew_time.as_ref() else {
            return true;
        };
        let duration = spec.lease_duration_seconds.unwrap_or(LEASE_SECS).max(1) as i64;
        (now - renew.0).num_seconds() > duration
    }

    fn parse(lease: &Lease) -> Option<NodeRecord> {
        let annotations = lease.metadata.annotations.as_ref()?;
        let name = annotations.get(ANN_NODE)?.clone();
        Some(NodeRecord {
            name,
            version: annotations.get(ANN_VERSION).cloned().unwrap_or_default(),
            since: annotations
                .get(ANN_SINCE)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            owner: annotations.get(ANN_OWNER).cloned().unwrap_or_default(),
            owner_addr: annotations.get(ANN_OWNER_ADDR).cloned().unwrap_or_default(),
        })
    }

    fn to_lease(record: &NodeRecord, name: String, now: DateTime<Utc>) -> Lease {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_REGISTRY.to_owned(), LABEL_REGISTRY_VALUE.to_owned());
        let mut annotations = BTreeMap::new();
        annotations.insert(ANN_NODE.to_owned(), record.name.clone());
        annotations.insert(ANN_OWNER.to_owned(), record.owner.clone());
        annotations.insert(ANN_OWNER_ADDR.to_owned(), record.owner_addr.clone());
        annotations.insert(ANN_VERSION.to_owned(), record.version.clone());
        annotations.insert(ANN_SINCE.to_owned(), record.since.to_string());
        Lease {
            metadata: ObjectMeta {
                name: Some(name),
                labels: Some(labels),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                acquire_time: Some(MicroTime(now)),
                holder_identity: Some(record.owner.clone()),
                lease_duration_seconds: Some(LEASE_SECS),
                lease_transitions: Some(0),
                renew_time: Some(MicroTime(now)),
            }),
        }
    }
}

#[async_trait]
impl NodeRegistry for K8sRegistry {
    async fn register(&self, record: NodeRecord) -> Result<(), String> {
        let api = self.api();
        let name = Self::lease_name(&record.name);
        let now = Utc::now();
        let lease = Self::to_lease(&record, name.clone(), now);
        match api.create(&PostParams::default(), &lease).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(response)) if response.code == 409 => {
                // 已存在:只有过期租约(进程崩溃残留)可由新连接接管。
                let existing = api
                    .get(&name)
                    .await
                    .map_err(|e| format!("读取既有租约失败: {e}"))?;
                if !Self::expired(&existing, now) {
                    return Err(format!("节点重名在线:{}", record.name));
                }
                let mut replacement = lease;
                replacement.metadata.resource_version = existing.metadata.resource_version.clone();
                api.replace(&name, &PostParams::default(), &replacement)
                    .await
                    .map_err(|e| format!("接管租约失败: {e}"))?;
                Ok(())
            }
            Err(e) => Err(format!("注册租约失败: {e}")),
        }
    }

    async fn remove(&self, name: &str, owner: &str) {
        let api = self.api();
        let lease_name = Self::lease_name(name);
        let Ok(existing) = api.get(&lease_name).await else {
            return;
        };
        let same_owner = existing
            .spec
            .as_ref()
            .and_then(|spec| spec.holder_identity.as_deref())
            == Some(owner);
        if same_owner {
            // Guard against deleting a lease that was taken over between the
            // get and the delete. A 409 conflict is the desired outcome here.
            let preconditions = Preconditions {
                resource_version: existing.metadata.resource_version.clone(),
                uid: existing.metadata.uid.clone(),
            };
            let _ = api
                .delete(
                    &lease_name,
                    &DeleteParams::default().preconditions(preconditions),
                )
                .await;
        }
    }

    async fn get(&self, name: &str) -> Option<NodeRecord> {
        let api = self.api();
        let lease_name = Self::lease_name(name);
        match api.get(&lease_name).await {
            Ok(lease) if !Self::expired(&lease, Utc::now()) => Self::parse(&lease),
            Ok(_) => None,
            Err(kube::Error::Api(response)) if response.code == 404 => None,
            Err(e) => {
                eprintln!("k8s 注册表读取失败({name}): {e}");
                None
            }
        }
    }

    async fn online(&self) -> Vec<String> {
        let api = self.api();
        let params =
            ListParams::default().labels(&format!("{LABEL_REGISTRY}={LABEL_REGISTRY_VALUE}"));
        let now = Utc::now();
        match api.list(&params).await {
            Ok(list) => {
                let mut names: Vec<String> = list
                    .items
                    .iter()
                    .filter(|lease| !Self::expired(lease, now))
                    .filter_map(Self::parse)
                    .map(|record| record.name)
                    .collect();
                names.sort();
                names
            }
            Err(e) => {
                eprintln!("k8s 注册表列表失败: {e}");
                Vec::new()
            }
        }
    }

    async fn renew(&self, name: &str, owner: &str, _now_ms: i64) -> Result<(), String> {
        let api = self.api();
        let lease_name = Self::lease_name(name);
        let existing = api
            .get(&lease_name)
            .await
            .map_err(|e| format!("读取租约失败: {e}"))?;
        let same_owner = existing
            .spec
            .as_ref()
            .and_then(|spec| spec.holder_identity.as_deref())
            == Some(owner);
        if !same_owner {
            return Err(format!("节点 {name} 不属于本实例"));
        }
        let patch = serde_json::json!({
            "spec": { "renewTime": MicroTime(Utc::now()) }
        });
        api.patch(&lease_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| format!("租约续约失败: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_names_are_deterministic_and_k8s_safe() {
        let a = K8sRegistry::lease_name("Home-Mac_01");
        let b = K8sRegistry::lease_name("home-mac_01");
        assert_eq!(a, b);
        assert!(a.starts_with("mixum-node-home-mac-01-"));
        assert!(a.len() <= 63);
        assert_ne!(a, K8sRegistry::lease_name("home-mac-02"));
    }

    #[test]
    fn expiry_uses_renew_time() {
        let now = Utc::now();
        let record = NodeRecord {
            name: "n".into(),
            version: "v".into(),
            since: 1,
            owner: "gw".into(),
            owner_addr: String::new(),
        };
        let mut lease = K8sRegistry::to_lease(&record, "l".into(), now);
        assert!(!K8sRegistry::expired(&lease, now));
        assert!(K8sRegistry::expired(
            &lease,
            now + chrono::Duration::seconds(LEASE_SECS as i64 + 5)
        ));
        lease.spec.as_mut().unwrap().renew_time = Some(MicroTime(now));
        assert!(!K8sRegistry::expired(&lease, now));
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    #[test]
    fn lease_name_is_deterministic_and_stable() {
        assert_eq!(
            K8sRegistry::lease_name("desktop-01"),
            K8sRegistry::lease_name("desktop-01")
        );
        assert_ne!(
            K8sRegistry::lease_name("desktop-01"),
            K8sRegistry::lease_name("desktop-02")
        );
    }

    #[test]
    fn lease_name_is_a_valid_dns_label() {
        for name in ["My Node!!", "节点", "", "a".repeat(200).as_str()] {
            let lease = K8sRegistry::lease_name(name);
            assert!(lease.len() <= 63, "{lease} too long");
            assert!(lease.starts_with("mixum-node-"), "{lease}");
            assert!(
                lease
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{lease} not a DNS label"
            );
            assert!(!lease.contains("--"), "{lease}");
            assert!(!lease.ends_with('-'), "{lease}");
        }
    }

    #[test]
    fn lease_name_collapses_specials_but_hashes_original() {
        assert!(K8sRegistry::lease_name("My Node!!").contains("-my-node-"));
        assert_ne!(
            K8sRegistry::lease_name("node a"),
            K8sRegistry::lease_name("node-a")
        );
    }
}
