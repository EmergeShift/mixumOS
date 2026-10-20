// gateway M3 e2e:两个 gateway 副本(gw-a/gw-b)+ k8s Lease 共享注册表 +
// mesh 跨副本路由。拓扑:
//   节点 a → gw-a,节点 b → gw-b;客户端连 gw-b 请求节点 a(跨副本),
//   以及连 gw-a 请求节点 b(反向),两条链路都要通。
//
// 前置:kind 集群(kind-mixum-e2e)、镜像 mixumos-e2e-rust:gw、
//       rust/target/debug/mixum-gateway(.exe)。
// 用法:node tests/integration/gateway-m3-e2e.mjs

import { execFileSync, spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

process.env.MSYS_NO_PATHCONV = "1";
const ROOT = fileURLToPath(new URL("../..", import.meta.url)).split("\\").join("/");
const GW_A = 17801, GW_B = 17802;
const MESH_A = 17811, MESH_B = 17812;
const INSTANCE_A = "gw-a", INSTANCE_B = "gw-b";
const MESH_TOKEN = "m3-e2e-token";
const NS = "mixumos";
const KUBECONFIG = `${ROOT}/build/kubeconfig-m3.yaml`;
const GW_BIN = existsSync(`${ROOT}/rust/target/debug/mixum-gateway.exe`)
  ? `${ROOT}/rust/target/debug/mixum-gateway.exe`
  : `${ROOT}/rust/target/debug/mixum-gateway`;

const children = [];
const containers = ["mixum-m3-a", "mixum-m3-b"];
const gateways = [];

function sh(cmd, args) {
  return execFileSync(cmd, args, { encoding: "utf8", env: { ...process.env } });
}
function cleanup() {
  for (const name of containers) {
    try { execFileSync("docker", ["rm", "-f", name], { stdio: "ignore" }); } catch { /* noop */ }
  }
  for (const child of children) {
    try { process.kill(child.pid); } catch { /* noop */ }
  }
  try {
    execFileSync("kubectl", ["--context", "kind-mixum-e2e", "delete", "lease", "-n", NS,
      "-l", "mixumos.io/registry=gateway", "--ignore-not-found"], { stdio: "ignore" });
  } catch { /* noop */ }
}
process.on("SIGINT", () => { cleanup(); process.exit(130); });

// ── 0. 环境准备 ──
mkdirSync(`${ROOT}/build`, { recursive: true });
const kubeconfig = sh("kubectl", ["--context", "kind-mixum-e2e", "config", "view", "--minify", "--flatten"]);
writeFileSync(KUBECONFIG, kubeconfig);
process.env.KUBECONFIG = KUBECONFIG;
sh("kubectl", ["--context", "kind-mixum-e2e", "delete", "lease", "-n", NS,
  "-l", "mixumos.io/registry=gateway", "--ignore-not-found"]);
rmSync(`${ROOT}/build/gw-m3-data-a`, { recursive: true, force: true });
  rmSync(`${ROOT}/build/gw-m3-data-b`, { recursive: true, force: true });
cleanup();

function startGateway(instance, listen, mesh, dataDir) {
  const log = `${ROOT}/build/${instance}.log`;
  writeFileSync(log, "");
  const fd = openSync(log, "a");
  const child = spawn(GW_BIN, [
    "--listen", `ws://127.0.0.1:${listen}`,
    "--data-dir", dataDir,
    "--instance-id", instance,
    "--registry", "k8s",
    "--kube-namespace", NS,
    "--mesh-listen", `127.0.0.1:${mesh}`,
    "--mesh-token", MESH_TOKEN,
    "--advertise", `ws://127.0.0.1:${mesh}`,
  ], {
    stdio: ["ignore", fd, fd],
    env: { ...process.env, KUBECONFIG, MIXUM_MESH_TOKEN: MESH_TOKEN },
  });
  children.push(child);
  return { child, log };
}

async function waitToken(entry) {
  for (let i = 0; i < 80; i++) {
    const text = existsSync(entry.log) ? readFileSync(entry.log, "utf8") : "";
    const matches = [...text.matchAll(/bootstrap token: (\S+)/g)];
    if (matches.length > 0) return matches[matches.length - 1][1];
    if (entry.child.exitCode !== null) break;
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`gateway 未就绪: ${entry.log}`);
}

function ws(url) {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  const pending = new Map();
  socket.addEventListener("message", (ev) => {
    if (typeof ev.data !== "string") return;
    const env = JSON.parse(ev.data);
    const waiter = pending.get(env.replyTo);
    if (waiter) { pending.delete(env.replyTo); waiter(env); }
  });
  const opened = new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve);
    socket.addEventListener("error", reject);
  });
  const call = (op, payload, node, timeout = 15000) => new Promise((resolve) => {
    const env = {
      v: 1, id: crypto.randomUUID(), op, ts: Date.now(),
      ...(node ? { node } : {}), ...(payload ? { payload } : {}),
    };
    const timer = setTimeout(() => {
      pending.delete(env.id);
      resolve({ op: `${op}.err`, payload: { message: "timeout" } });
    }, timeout);
    pending.set(env.id, (reply) => { clearTimeout(timer); resolve(reply); });
    socket.send(JSON.stringify(env));
  });
  return { socket, opened, call };
}

async function main() {
  const dataA = `${ROOT}/build/gw-m3-data-a`;
  const dataB = `${ROOT}/build/gw-m3-data-b`;
  const a = startGateway(INSTANCE_A, GW_A, MESH_A, dataA);
  const token = await waitToken(a);
  console.log("1. gw-a 就绪 ✓");
  // gw-b 用独立 state(避免 bootstrap 轮换互踢),但复制同一 identity 文件。
  mkdirSync(dataB, { recursive: true });
  for (const name of ["identity.key", "tls-cert.pem", "tls-key.pem"]) {
    copyFileSync(`${dataA}/${name}`, `${dataB}/${name}`);
  }
  const b = startGateway(INSTANCE_B, GW_B, MESH_B, dataB);
  await waitToken(b);
  console.log("2. gw-b 就绪(共享签发根)✓");

  const admin = ws(`ws://127.0.0.1:${GW_A}`);
  await admin.opened;
  const hello = await admin.call("session.hello", { token, client: "m3-e2e/0.1" });
  if (!hello.op?.endsWith(".ok")) throw new Error(`hello 失败 ${JSON.stringify(hello)}`);
  console.log("3. admin hello ✓");

  const nodeTokens = {};
  for (const name of ["a", "b"]) {
    const issued = await admin.call("gateway.token.issue", { kind: "node", name });
    nodeTokens[name] = issued.payload?.token;
    if (!nodeTokens[name]) throw new Error(`节点令牌签发失败 ${JSON.stringify(issued)}`);
  }
  console.log("4. 节点令牌签发 ✓");

  for (const [name, port] of [["a", GW_A], ["b", GW_B]]) {
    execFileSync("docker", [
      "run", "-d", "--name", `mixum-m3-${name}`,
      "-e", `MIXUM_GATEWAY_URL=ws://host.docker.internal:${port}`,
      "-e", `MIXUM_GATEWAY_NAME=${name}`,
      "-e", `MIXUM_GATEWAY_TOKEN=${nodeTokens[name]}`,
      "mixumos-e2e-rust:gw",
    ], { stdio: "ignore" });
  }
  console.log("5. 节点容器启动 ✓ (a→gw-a, b→gw-b)");

  // 两个副本都从共享注册表看到 a/b。
  const clientB = ws(`ws://127.0.0.1:${GW_B}`);
  await clientB.opened;
  await clientB.call("session.hello", { token, client: "m3-e2e-b/0.1" });
  let nodes = [];
  for (let i = 0; i < 60; i++) {
    const listed = await clientB.call("gateway.node.list", {});
    nodes = listed.payload?.nodes ?? [];
    if (nodes.includes("a") && nodes.includes("b")) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  if (!(nodes.includes("a") && nodes.includes("b"))) {
    throw new Error(`共享注册表未收敛: ${JSON.stringify(nodes)}`);
  }
  console.log(`6. 共享注册表(gw-b 视角)✓ ${JSON.stringify(nodes)}`);

  const leases = sh("kubectl", ["--context", "kind-mixum-e2e", "get", "lease", "-n", NS,
    "-l", "mixumos.io/registry=gateway", "-o", "name"]).trim().split("\n").filter(Boolean);
  if (leases.length < 2) throw new Error(`Lease 数量不足: ${leases.length}`);
  console.log(`7. k8s Lease 落盘 ✓ ${leases.length} 条`);

  // 跨副本:客户端在 B,请求 owner 在 A 的节点 a。
  const events = [];
  clientB.socket.addEventListener("message", (ev) => {
    if (typeof ev.data !== "string") return;
    const env = JSON.parse(ev.data);
    if (env.op === "task.event") events.push(env.payload);
  });
  await clientB.call("task.events", {}, "a");
  const submitted = await clientB.call("task.submit", {
    instruction: "echo cross-replica",
    agent: "shell",
    params: { cmd: "echo cross-replica" },
  }, "a");
  if (!submitted.op?.endsWith(".ok")) throw new Error(`跨副本提交失败 ${JSON.stringify(submitted)}`);
  const taskId = submitted.payload.id;
  const stateOf = (e) => (typeof e.data === "string" ? e.data : e.data?.state);
  let completed = false;
  for (let i = 0; i < 60 && !completed; i++) {
    await new Promise((r) => setTimeout(r, 250));
    completed = events.some((e) => e.taskId === taskId && e.type === "state" && stateOf(e) === "completed");
  }
  if (!completed) throw new Error("跨副本任务完成事件未到达");
  console.log("8. B→A 跨副本任务 + 事件流 ✓");

  // 反向:A 上请求 owner 在 B 的节点 b。
  const clientA = ws(`ws://127.0.0.1:${GW_A}`);
  await clientA.opened;
  await clientA.call("session.hello", { token, client: "m3-e2e-a/0.1" });
  const reverse = await clientA.call("task.submit", {
    instruction: "echo reverse",
    agent: "shell",
    params: { cmd: "echo reverse" },
  }, "b");
  if (!reverse.op?.endsWith(".ok")) throw new Error(`反向跨副本提交失败 ${JSON.stringify(reverse)}`);
  let reverseState = "";
  for (let i = 0; i < 40 && reverseState !== "completed"; i++) {
    await new Promise((r) => setTimeout(r, 250));
    const list = await clientA.call("task.list", {}, "b");
    reverseState = (list.payload?.tasks ?? []).find((t) => t.id === reverse.payload.id)?.state ?? "";
  }
  if (reverseState !== "completed") throw new Error(`反向任务未完成: ${reverseState}`);
  console.log("9. A→B 反向跨副本任务 ✓");

  // 节点 a 掉线 → owner 侧移除 Lease,另一个副本看到收敛。
  execFileSync("docker", ["rm", "-f", "mixum-m3-a"], { stdio: "ignore" });
  let remaining = [];
  for (let i = 0; i < 40; i++) {
    const listed = await clientB.call("gateway.node.list", {});
    remaining = listed.payload?.nodes ?? [];
    if (!remaining.includes("a")) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  if (remaining.includes("a")) throw new Error(`节点 a 掉线后仍在线: ${JSON.stringify(remaining)}`);
  console.log("10. 节点掉线跨副本清理 ✓");

  console.log("PASS gateway M3:共享 Lease 注册表 + 双向跨副本路由 + 掉线清理");
}

main()
  .then(() => { cleanup(); process.exit(0); })
  .catch((error) => { console.error(`FAIL: ${error.message}`); cleanup(); process.exit(1); });
