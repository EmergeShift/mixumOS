// k8s operator kind e2e:MixumTask → 任务 Pod → 状态回写 → TTL 回收。
//
// 前置(Linux/macOS/Windows Git Bash 均可):
//   - kind CLI 在 PATH,或 KIND=/opt/homebrew/bin/kind(Windows:KIND="$HOME/kind.exe")
//   - 集群: kind create cluster --name mixum-e2e(或 KIND_CLUSTER 指定名字)
//   - operator 镜像: 先产出静态二进制(cloud/operator/Dockerfile 只 COPY):
//       docker run --rm -v "$PWD/rust:/src" -w /src \
//         -v "$PWD/cloud/operator:/out" \
//         -v mixum-dist-cargo:/cargo-cache -e CARGO_HOME=/cargo-cache \
//         -v mixum-cargo-target:/target -e CARGO_TARGET_DIR=/target rust:1-alpine \
//         sh -c 'apk add musl-dev pkgconf && \
//           cargo build --release -p mixum-operator && \
//           cp /target/release/mixum-operator /out/'
//   - runner 镜像(二选一,产物由 make dist 生成,<ver> 取自 PKGBUILD):
//       OCI:    docker load -i dist/out/mixumos-<ver>-x86_64.oci.tar
//       rootfs: docker import --platform amd64 dist/out/rootfs-x86_64.tar \
//                 mixumos:<ver>-x86_64
//     默认 RUNNER_IMAGE=mixumos:<ver>-x86_64,可用环境变量覆盖。
//   - OCI 镜像自带 ENTRYPOINT /entrypoint.sh,rootfs 导入镜像没有;任务容器
//     显式 command + 空 args 覆盖两者,均直起 /usr/bin/mixumd。
// 可选:OCI_RUNNER_IMAGE=<oci tag> 追加一次 OCI 产物用例(默认只验 RUNNER_IMAGE)。
// 用法: KIND=kind RUNNER_IMAGE=... node tests/integration/operator-e2e.mjs
process.env.MSYS_NO_PATHCONV = "1";

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const VERSION =
  readFileSync(join(ROOT, "dist", "pkgs", "mixum-base", "PKGBUILD"), "utf8")
    .match(/^pkgver=(.+)$/m)?.[1]
    ?.trim() ?? "dev";
const KIND = process.env.KIND ?? "kind";
const CLUSTER = process.env.KIND_CLUSTER ?? "mixum-e2e";
const CTX = process.env.KUBE_CONTEXT ?? `kind-${CLUSTER}`;
const NS = "mixumos";
const OPERATOR_IMAGE = process.env.OPERATOR_IMAGE ?? "ghcr.io/mixumos/mixum-operator:e2e";
const RUNNER_IMAGE = process.env.RUNNER_IMAGE ?? `mixumos:${VERSION}-x86_64`;
const OCI_RUNNER_IMAGE = process.env.OCI_RUNNER_IMAGE ?? "";
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
const imageExists = (ref) => sh("docker", ["image", "inspect", ref]).status === 0;

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

const taskYaml = (name, instruction, extra = "", image = RUNNER_IMAGE) => `\
apiVersion: mixumos.io/v1
kind: MixumTask
metadata:
  name: ${name}
spec:
  instruction: ${JSON.stringify(instruction)}
  agent: shell
  runnerImage: ${image}
  ttlSecondsAfterFinished: 15
${extra}`;

try {
  // 1. 前置自检 + 构建 operator 镜像并装载进 kind(连同 runner 镜像)。
  console.log("1. 镜像装载");
  if (!existsSync(join(ROOT, "cloud", "operator", "mixum-operator"))) {
    throw new Error(
      "缺 cloud/operator/mixum-operator;构建命令见本文件头注释(operator 镜像只在 Dockerfile 里 COPY 该二进制)",
    );
  }
  if (!imageExists(RUNNER_IMAGE)) {
    throw new Error(
      `runner 镜像 ${RUNNER_IMAGE} 不存在;先 make dist 后二选一导入:\n` +
        `  OCI:    docker load -i dist/out/mixumos-${VERSION}-x86_64.oci.tar\n` +
        `  rootfs: docker import --platform amd64 dist/out/rootfs-x86_64.tar ${RUNNER_IMAGE}\n` +
        "  或设置 RUNNER_IMAGE 指向已有镜像",
    );
  }
  expect(sh("docker", ["build", "-t", OPERATOR_IMAGE, join(ROOT, "cloud", "operator")]).status === 0, "operator 镜像构建");
  expect(sh(KIND, ["load", "docker-image", OPERATOR_IMAGE, "--name", CLUSTER]).status === 0, "operator 镜像入集群");
  expect(sh(KIND, ["load", "docker-image", RUNNER_IMAGE, "--name", CLUSTER]).status === 0, "runner 镜像入集群");

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
    "任务容器显式直起 mixumd",
  );
  // API server 会把空数组规范化掉:缺失与 [] 都表示未继承镜像 CMD。
  const podArgs = pod.spec.containers[0].args;
  expect(
    podArgs === undefined || (Array.isArray(podArgs) && podArgs.length === 0),
    "args 为空(不继承镜像 CMD)",
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

  // 4b. 可选:OCI 产物用例(自带 ENTRYPOINT /entrypoint.sh,验证 command 覆盖)。
  if (OCI_RUNNER_IMAGE) {
    console.log("4b. OCI 镜像用例");
    if (!imageExists(OCI_RUNNER_IMAGE)) {
      throw new Error(
        `OCI 镜像 ${OCI_RUNNER_IMAGE} 不存在;docker load -i dist/out/mixumos-${VERSION}-x86_64.oci.tar`,
      );
    }
    expect(
      sh(KIND, ["load", "docker-image", OCI_RUNNER_IMAGE, "--name", CLUSTER]).status === 0,
      "OCI 镜像入集群",
    );
    const applyOci = kubectl(["apply", "-f", "-"], {
      input: taskYaml("e2e-oci", "echo oci-check", "", OCI_RUNNER_IMAGE),
    });
    expect(applyOci.status === 0, "MixumTask e2e-oci 提交", applyOci.out);
    await waitFor(async () => {
      const r = kubectl(["get", "mtask", "e2e-oci", "-o", "json"]);
      return r.status === 0 && JSON.parse(r.out).status?.state === "completed";
    }, 180_000, "e2e-oci 完成");
    expect(true, "OCI 镜像任务 completed(显式 command 覆盖 ENTRYPOINT)");
  }

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
  console.log("6. TTL 回收");
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
