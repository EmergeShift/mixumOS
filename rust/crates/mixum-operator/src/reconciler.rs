//! 调谐器:MixumTask 的期望状态 ↔ 任务 Pod 的实际状态。

use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use kube::{
    api::{Api, DeleteParams, ObjectMeta, Patch, PatchParams, PostParams},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        events::{Event, EventType, Recorder, Reporter},
        watcher::{self, Config},
    },
    CustomResource, Resource, ResourceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// MixumTask CRD 的 Rust 类型(与 cloud/k8s/10-crd.yaml 对齐)。
#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    kind = "MixumTask",
    group = "mixumos.io",
    version = "v1",
    namespaced,
    status = "MixumTaskStatus",
    shortname = "mtask"
)]
#[serde(rename_all = "camelCase")]
pub struct MixumTaskSpec {
    pub instruction: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    /// 目标架构:空 = 不限;arm64 / amd64 注入 nodeSelector。
    #[serde(default)]
    pub arch: String,
    #[serde(default = "default_image")]
    pub runner_image: String,
    /// 工作区与快照 PVC;跨任务指向同一 PVC 即云端接力。
    #[serde(default)]
    pub snapshot_pvc: Option<String>,
    #[serde(default = "default_ttl")]
    pub ttl_seconds_after_finished: i32,
}

fn default_agent() -> String {
    "shell".into()
}
fn default_image() -> String {
    "ghcr.io/mixumos/mixumos:latest".into()
}
fn default_ttl() -> i32 {
    3600
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MixumTaskStatus {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 终态时间(RFC3339),TTL 回收的起点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Kubernetes API: {0}")]
    Kube(#[from] kube::Error),
}

pub struct Context {
    pub client: Client,
    pub reporter: Reporter,
}

const FIELD_MANAGER: &str = "mixum-operator";
/// 任务容器内的 agentd 端口(wss)。
const AGENTD_PORT: i32 = 7601;

fn pod_name(task: &MixumTask) -> String {
    format!("mixum-task-{}", (task.name_any()).to_lowercase())
}

fn task_pod(task: &MixumTask) -> Result<k8s_openapi::api::core::v1::Pod, Error> {
    let labels = vec![
        ("app.kubernetes.io/name", "mixum-task"),
        ("mixumos.io/task", task.name_any().as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<_>();

    let mut pod = k8s_openapi::api::core::v1::Pod {
        metadata: ObjectMeta {
            name: Some(pod_name(task)),
            labels: Some(labels),
            owner_references: Some(vec![task.controller_owner_ref(&()).ok_or_else(|| {
                Error::Kube(kube::Error::Api(kube::error::ErrorResponse {
                    code: 422,
                    message: "MixumTask 缺少 uid,无法建立从属引用".into(),
                    reason: "Invalid".into(),
                    status: "Failure".into(),
                }))
            })?]),
            ..Default::default()
        },
        spec: Default::default(),
        status: None,
    };

    let mut env = vec![
        k8s_openapi::api::core::v1::EnvVar {
            name: "MIXUM_RUN_ONCE".into(),
            value: Some("1".into()),
            ..Default::default()
        },
        k8s_openapi::api::core::v1::EnvVar {
            name: "MIXUM_TASK_ID".into(),
            value: Some(task.name_any()),
            ..Default::default()
        },
        k8s_openapi::api::core::v1::EnvVar {
            name: "MIXUM_INSTRUCTION".into(),
            value: Some(task.spec.instruction.clone()),
            ..Default::default()
        },
    ];

    let mut container = k8s_openapi::api::core::v1::Container {
        name: "mixumos".into(),
        image: Some(task.spec.runner_image.clone()),
        image_pull_policy: Some("IfNotPresent".into()),
        ports: Some(vec![k8s_openapi::api::core::v1::ContainerPort {
            name: Some("wss".into()),
            container_port: AGENTD_PORT,
            ..Default::default()
        }]),
        ..Default::default()
    };

    if !task.spec.agent.is_empty() {
        env.push(k8s_openapi::api::core::v1::EnvVar {
            name: "MIXUM_AGENT".into(),
            value: Some(task.spec.agent.clone()),
            ..Default::default()
        });
    }
    container.env = Some(env);

    let mut volumes = Vec::new();
    let mut mounts = Vec::new();
    if let Some(pvc) = &task.spec.snapshot_pvc {
        volumes.push(k8s_openapi::api::core::v1::Volume {
            name: "mixum-data".into(),
            persistent_volume_claim: Some(
                k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                    claim_name: pvc.clone(),
                    ..Default::default()
                },
            ),
            ..Default::default()
        });
        mounts.push(k8s_openapi::api::core::v1::VolumeMount {
            name: "mixum-data".into(),
            mount_path: "/var/lib/mixum".into(),
            ..Default::default()
        });
    }
    if !volumes.is_empty() {
        container.volume_mounts = Some(mounts);
    }

    pod.spec = Some(k8s_openapi::api::core::v1::PodSpec {
        containers: vec![container],
        restart_policy: Some("Never".into()),
        // 混合架构节点池:spec.arch 非空时注入 nodeSelector。
        node_selector: if task.spec.arch.is_empty() {
            None
        } else {
            Some(
                vec![("kubernetes.io/arch".to_string(), task.spec.arch.clone())]
                    .into_iter()
                    .collect(),
            )
        },
        volumes: if volumes.is_empty() {
            None
        } else {
            Some(volumes)
        },
        ..Default::default()
    });
    Ok(pod)
}

async fn set_status(
    tasks: &Api<MixumTask>,
    name: &str,
    status: MixumTaskStatus,
) -> Result<(), Error> {
    // Server-Side Apply 的负载必须自带 apiVersion/kind,否则 400。
    let patch = serde_json::json!({
        "apiVersion": "mixumos.io/v1",
        "kind": "MixumTask",
        "status": status,
    });
    tasks
        .patch_status(
            name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Apply(&patch),
        )
        .await?;
    Ok(())
}

/// 主调谐:确保每个 MixumTask 恰有一个任务 Pod,并把 Pod 状态回写 status。
pub async fn reconcile(task: Arc<MixumTask>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = task.namespace().unwrap_or_else(|| "mixumos".into());
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let tasks: Api<MixumTask> = Api::namespaced(ctx.client.clone(), &ns);

    let name = pod_name(&task);
    let pod = pods.get_opt(&name).await?;

    match pod {
        None => {
            if task.status.as_ref().map(|s| s.state.as_str()) == Some("completed") {
                return Ok(Action::await_change()); // 已终态,不重建
            }
            pods.create(&PostParams::default(), &task_pod(&task)?)
                .await?;
            recorder(&ctx, &task)
                .publish(Event {
                    type_: EventType::Normal,
                    reason: "PodCreated".into(),
                    note: Some(format!("任务容器已创建: {name}")),
                    action: "CreatePod".into(),
                    secondary: None,
                })
                .await
                .ok();
            set_status(
                &tasks,
                &task.name_any(),
                MixumTaskStatus {
                    state: "pending".into(),
                    pod_name: Some(name),
                    message: None,
                    finished_at: None,
                },
            )
            .await?;
        }
        Some(p) => {
            let state = match &p.status {
                Some(st) => match st.phase.as_deref() {
                    Some("Running") => "running",
                    Some("Succeeded") => "completed",
                    Some("Failed") => "failed",
                    _ => "pending",
                },
                None => "pending",
            };
            let cur = task.status.as_ref().map(|s| s.state.as_str()).unwrap_or("");
            if cur != state {
                let terminal = matches!(state, "completed" | "failed");
                set_status(
                    &tasks,
                    &task.name_any(),
                    MixumTaskStatus {
                        state: state.into(),
                        pod_name: Some(name.clone()),
                        message: p.status.as_ref().and_then(|s| s.reason.clone()),
                        finished_at: terminal
                            .then(now_rfc3339)
                            .or_else(|| task.status.as_ref().and_then(|s| s.finished_at.clone())),
                    },
                )
                .await?;
            }

            // TTL 回收:终态超过 ttl_seconds_after_finished 后删除任务 Pod。
            if matches!(state, "completed" | "failed") {
                let finished = task
                    .status
                    .as_ref()
                    .and_then(|s| s.finished_at.as_deref())
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
                if let Some(finished) = finished {
                    let ttl = chrono::Duration::seconds(i64::from(
                        task.spec.ttl_seconds_after_finished.max(0),
                    ));
                    if Utc::now() - finished.with_timezone(&Utc) > ttl {
                        pods.delete(&name, &DeleteParams::default()).await?;
                        recorder(&ctx, &task)
                            .publish(Event {
                                type_: EventType::Normal,
                                reason: "PodTtlDeleted".into(),
                                note: Some(format!("任务容器已按 TTL 回收: {name}")),
                                action: "DeletePod".into(),
                                secondary: None,
                            })
                            .await
                            .ok();
                        return Ok(Action::await_change());
                    }
                }
            }
        }
    }
    Ok(Action::requeue(std::time::Duration::from_secs(30)))
}

fn recorder(ctx: &Context, task: &MixumTask) -> Recorder {
    Recorder::new(
        ctx.client.clone(),
        ctx.reporter.clone(),
        task.object_ref(&()),
    )
}

/// 组装 Controller(供 main 调用)。
pub async fn run(client: Client, namespace: &str) -> Result<(), Error> {
    let tasks: Api<MixumTask> = Api::namespaced(client.clone(), namespace);
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), namespace);
    let ctx = Context {
        client: client.clone(),
        reporter: Reporter {
            controller: "mixum-operator".into(),
            instance: None,
        },
    };
    Controller::new(tasks, watcher::Config::default())
        .owns(pods, Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::new(ctx))
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::error!("调谐循环错误: {e:?}");
            }
        })
        .await;
    Ok(())
}

fn error_policy(obj: Arc<MixumTask>, err: &Error, _ctx: Arc<Context>) -> Action {
    tracing::warn!("调谐失败 task={} err={}", obj.name_any(), err);
    Action::requeue(std::time::Duration::from_secs(60))
}

/// 当前时间(RFC3339),状态时间戳用。
pub fn now_rfc3339() -> String {
    let now: DateTime<Utc> = Utc::now();
    now.to_rfc3339()
}
