# mixumOS 云端部署(k8s)

> 状态:清单与 operator 随阶段 5 落地;镜像产物来自 `make dist`
> (dist/out/mixumos-<ver>-<arch>.oci.tar,双架构 OCI index)。

## 组成

```
cloud/k8s/          直接可 apply 的清单
  00-namespace.yaml    mixumos 命名空间
  10-crd.yaml          MixumTask CRD(一条指令 = 一个任务容器)
  20-rbac.yaml         operator 的 RBAC(管理 MixumTask 与 Pod)
  30-operator.yaml     mixum-operator Deployment(Rust,kube-rs)
  40-server.yaml       常驻 mixumos 容器(长会话/设备对接形态)
  50-service.yaml      WSS Service + Ingress(TLS 透传,协议见 docs/protocol.md)
  60-gateway.yaml      多副本 gateway Deployment/RBAC/headless mesh Service(M3)
cloud/operator/     Rust operator 源码(kube-rs)
cloud/gateway/      gateway 镜像 Dockerfile + build.sh（M3 部署清单用）
```

## 两种任务形态

| 形态 | 载体 | 适用 |
|------|------|------|
| 常驻容器 | 40-server Deployment | 设备长期对接一台容器(个人/mac 配对) |
| 一次性任务 | MixumTask CRD → operator 建 Pod | 云端临时跑指令,跑完回收(类 Manus) |

MixumTask 字段:`instruction`、`agent`、`runnerImage`(默认
`ghcr.io/mixumos/mixumos:<ver>`)、`snapshotPVC`(快照/工作区卷,PVC 模板
由用户预建,跨 Pod 迁移即「云端接力」)、`ttlSecondsAfterFinished`。

节点池:arm64 与 x86_64 混合池均可跑(镜像为 OCI index 双 manifest);
指定架构用 `mixumos.io/arch: "<arm64|amd64>"` nodeSelector(operator 从
MixumTask 的 `arch` 字段注入)。

## 安全

- Ingress 终止 TLS 时必须透传 WSS(注释里有 nginx/traefik 写法);
  生产建议在 Ingress 后仍保留容器内 TLS 1.3(端到端),网关只做路由;
- 能力令牌通过 Secret 注入,不在清单中明文;
- Pod 默认非特权;要启用内存快照(CRIU)的节点用
  `mixumos.io/privileged: "true"` 污点 + 对应容忍(阶段 5 变体镜像)。
