// k8s operator kind e2e:MixumTask → 任务 Pod → 状态回写 → TTL 回收。
//
// 前置:
//   kind 集群:  "$HOME/kind.exe" create cluster --name mixum-e2e(上下文 kind-mixum-e2e)
//   operator 产物: cloud/operator/mixum-operator(rust:1-alpine cargo build --release)
//   runner 镜像: docker images 里有 mixumos:0.1.0-x86_64(dist 双架构产物)
// 链路: 构建/装载镜像 → apply 清单(namespace/CRD/RBAC/operator)→
//       提交 MixumTask → 断言 Pod 创建+Succeeded+状态回写 completed →
//       arch 字段注入 nodeSelector → TTL 到期 Pod 回收。
// 用法: node tests/integration/operator-e2e.mjs
process.env.MSYS_NO_PATHCONV = "1";

import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const KIND = join(homedir(), "kind.exe");
const CTX = "kind-mixum-e2e";
const NS = "mixumos";
const OPERATOR_IMAGE = "ghcr.io/mixumos/mixum-operator:e2e";
const RUNNER_IMAGE = "mixumos:0.1.0-x86_64";
const BUILD = join(ROOT, "build");

function sh(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: "utf8", ...opts });
  return { status: r.status, out: (r.stdout ?? "") + (r.stderr ?? "") };
}

function kubectl(args, opts = {}) {
  return sh("kubectl", ["--context", CTX, "-n", NS, ...args], opts);
}

function expect(cond, message, detail = "") {
  if (!cond) throw new Error(`断言失败: ${message}${detail ? ` — ${detail.trim().slice(-400)}` : ""}`);
  console.log(`  ok: ${message}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e.message;
    }
    await sleep(2000);
  }
  throw new Error(`超时: ${label}(${last})`);
}

const taskYaml = (name, instruction, extra = "") => `\
apiVersion: mixumos.io/v1
kind: MixumTask
metadata:
  name: ${name}
spec:
  instruction: ${JSON.stringify(instruction)}
  agent: shell
  runnerImage: ${RUNNER_IMAGE}
  ttlSecondsAfterFinished: 15
${extra}`;

try {
  // 1. 构建 operator 镜像并装载进 kind(连同 runner 镜像)。
  console.log("1. 镜像装载");
  expect(sh("docker", ["build", "-t", OPERATOR_IMAGE, join(ROOT, "cloud", "operator")]).status === 0, "operator 镜像构建");
  expect(sh(KIND, ["load", "docker-image", OPERATOR_IMAGE, "--name", "mixum-e2e"]).status === 0, "operator 镜像入集群");
  expect(sh(KIND, ["load", "docker-image", RUNNER_IMAGE, "--name", "mixum-e2e"]).status === 0, "runner 镜像入集群");

  // 2. apply 清单(namespace/CRD/RBAC/operator,operator 镜像换 e2e tag)。
  console.log("2. 清单 apply");
  for (const f of ["00-namespace.yaml", "10-crd.yaml", "20-rbac.yaml"]) {
    const r = kubectl(["apply", "-f", join(ROOT, "cloud", "k8s", f)]);
    expect(r.status === 0, f, r.out);
  }
  // CRD 建立(established)是异步的:不等就提交任务会撞 no matches for kind。
  const est = kubectl(["wait", "--for=condition=established", "crd/mixumtasks.mixumos.io", "--timeout=60s"]);
  expect(est.status === 0, "CRD established", est.out);
  mkdirSync(BUILD, { recursive: true });
  const deployYaml = readFileSync(join(ROOT, "cloud", "k8s", "30-operator.yaml"), "utf8")
    .replace("ghcr.io/mixumos/mixum-operator:latest", OPERATOR_IMAGE);
  const deployFile = join(BUILD, "operator-e2e.yaml");
  writeFileSync(deployFile, deployYaml);
  expect(kubectl(["apply", "-f", deployFile]).status === 0, "30-operator(镜像=e2e)");
  const rollout = waitFor(
    () => kubectl(["rollout", "status", "deploy/mixum-operator", "--timeout=10s"]).status === 0,
    180_000,
    "operator 就绪",
  );
  await rollout;
  expect(true, "operator Deployment 就绪");

  // 3. 提交一次性任务:状态回写 completed,Pod Succeeded。
  console.log("3. MixumTask 全链");
  const apply1 = kubectl(["apply", "-f", "-"], {
    input: taskYaml("e2e-echo", "echo mixum-operator-e2e-ok"),
  });
  expect(apply1.status === 0, "MixumTask e2e-echo 提交", apply1.out);
  const state3 = await waitFor(async () => {
    const r = kubectl(["get", "mtask", "e2e-echo", "-o", "json"]);
    if (r.status !== 0) return null;
    return JSON.parse(r.out).status?.state === "completed" ? "completed" : null;
  }, 180_000, "e2e-echo 完成");
  expect(state3 === "completed", "状态回写 completed");
  const podJson = kubectl(["get", "pod", "mixum-task-e2e-echo", "-o", "json"]);
  expect(podJson.status === 0, "任务 Pod 存在");
  const pod = JSON.parse(podJson.out);
  expect(pod.status.phase === "Succeeded", `Pod phase=${pod.status.phase}`);
  expect(
    pod.spec.containers[0].command?.[0] === "/usr/bin/mixumd",
    "任务容器显式直起 mixumd(dist 镜像无 ENTRYPOINT)",
  );

  // 4. arch 字段注入 nodeSelector。
  console.log("4. 架构亲和");
  const apply2 = kubectl(["apply", "-f", "-"], {
    input: taskYaml("e2e-arch", "echo arch-check", "  arch: amd64\n"),
  });
  expect(apply2.status === 0, "MixumTask e2e-arch 提交", apply2.out);
  await waitFor(async () => {
    const r = kubectl(["get", "pod", "mixum-task-e2e-arch", "-o", "json"]);
    if (r.status !== 0) return null;
    return JSON.parse(r.out).spec.nodeSelector?.["kubernetes.io/arch"] === "amd64";
  }, 120_000, "e2e-arch nodeSelector");
  expect(true, "arch=amd64 注入 nodeSelector");
  await waitFor(async () => {
    const r = kubectl(["get", "mtask", "e2e-arch", "-o", "json"]);
    return r.status === 0 && JSON.parse(r.out).status?.state === "completed";
  }, 180_000, "e2e-arch 完成");
  expect(true, "e2e-arch 状态回写 completed");

  // 5. 快照卷挂载:snapshotPvc 字段 → 任务 Pod 挂 PVC 到 /var/lib/mixum。
  console.log("5. 快照卷挂载");
  expect(
    kubectl(["apply", "-f", "-"], {
      input: `\
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: e2e-pvc
spec:
  accessModes: ["ReadWriteOnce"]
  resources: { requests: { storage: 1Gi } }
`,
    }).status === 0,
    "PVC e2e-pvc 创建",
  );
  const apply3 = kubectl(["apply", "-f", "-"], {
    input: taskYaml("e2e-pvc", "echo pvc-check", "  snapshotPvc: e2e-pvc\n"),
  });
  expect(apply3.status === 0, "MixumTask e2e-pvc 提交", apply3.out);
  await waitFor(async () => {
    const r = kubectl(["get", "pod", "mixum-task-e2e-pvc", "-o", "json"]);
    if (r.status !== 0) return null;
    const pod = JSON.parse(r.out);
    const mounted = (pod.spec.volumes ?? []).some(
      (v) => v.persistentVolumeClaim?.claimName === "e2e-pvc",
    );
    return mounted &&
      pod.spec.containers[0].volumeMounts?.some((m) => m.mountPath === "/var/lib/mixum")
      ? pod
      : null;
  }, 120_000, "e2e-pvc 卷挂载");
  expect(true, "snapshotPvc 挂载到 /var/lib/mixum");
  await waitFor(async () => {
    const r = kubectl(["get", "mtask", "e2e-pvc", "-o", "json"]);
    return r.status === 0 && JSON.parse(r.out).status?.state === "completed";
  }, 180_000, "e2e-pvc 完成");
  expect(true, "e2e-pvc 状态回写 completed");

  // 6. TTL 回收:终态 15s 后 Pod 被删。
  console.log("5. TTL 回收");
  await waitFor(
    () => kubectl(["get", "pod", "mixum-task-e2e-echo", "-o", "json"]).status !== 0,
    120_000,
    "TTL 回收 e2e-echo Pod",
  );
  expect(true, "终态 Pod 已按 TTL 回收");
  // 重建防护:已终态的任务不重建 Pod。
  await sleep(10_000);
  expect(kubectl(["get", "pod", "mixum-task-e2e-echo", "-o", "json"]).status !== 0, "已终态任务不重建 Pod");

  console.log("operator-e2e PASS");
} finally {
  // 清理任务对象(保留集群供 gateway M3 使用)。
  kubectl(["delete", "mtask", "--all", "--wait=false"]);
}
