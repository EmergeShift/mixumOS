// mixum-ctl 电力钩子 e2e:真实 CLI 二进制走「合盖迁移」全链路。
//
// 拓扑(sync-e2e 同款镜像):device 容器(发布 17611)← mixum-ctl(宿主)WSS 直连;
//   cloud 容器(发布 17612,仅宿主验证用)与 device 同网,device 侧 snap.push 直达。
// 链路:power --simulate-sleep → snap.create(hybrid,auto) → snap.push(容器间) →
//       cloud snap.tree 落树;二次运行产生新节点,cloud 树应含两个节点。
// 前置:cargo build -p mixum-ctl(宿主);镜像 mixumos-e2e-rust。
// 用法: node tests/integration/power-e2e.mjs [devicePort] [cloudPort]
process.env.MSYS_NO_PATHCONV = "1"; // docker 参数含容器路径,禁 git-bash 转换

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const DEVICE_PORT = process.argv[2] ?? 17611;
const CLOUD_PORT = process.argv[3] ?? 17612;
const DEVICE = `wss://127.0.0.1:${DEVICE_PORT}`;
const CLOUD_HOST = `wss://127.0.0.1:${CLOUD_PORT}`;
const REMOTE = "wss://mixum-power-cloud:7601";
const CTL =
  process.platform === "win32"
    ? join(ROOT, "rust", "target", "debug", "mixum-ctl.exe")
    : join(ROOT, "rust", "target", "debug", "mixum-ctl");

function sh(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: "utf8", ...opts });
  if (r.error) throw r.error;
  // docker logs 的 eprintln 走 stderr,合并两个流供日志解析。
  return (r.stdout ?? "") + (r.stderr ?? "");
}

function expect(cond, message) {
  if (!cond) throw new Error(`断言失败: ${message}`);
  console.log(`  ok: ${message}`);
}

function containersUp() {
  const out = sh("docker", ["ps", "--format", "{{.Names}}"]);
  return { device: out.includes("mixum-power-device"), cloud: out.includes("mixum-power-cloud") };
}

function logsHave(name) {
  const logs = sh("docker", ["logs", name]);
  return {
    token: logs.match(/bootstrap token: (\S+)/)?.[1],
    fingerprint: logs.match(/fingerprint: (\S+)/)?.[1],
  };
}

// 云端树验证用的最小 WS 会话(sync-e2e 同款信封语义)。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
class Session {
  constructor(url, token) {
    this.url = url;
    this.token = token;
    this.pending = new Map();
  }
  async connect() {
    this.ws = new WebSocket(this.url);
    this.ws.addEventListener("message", (ev) => {
      const env = JSON.parse(String(ev.data));
      const waiter = this.pending.get(env.replyTo);
      if (waiter) {
        this.pending.delete(env.replyTo);
        waiter(env);
      }
    });
    await new Promise((resolve, reject) => {
      this.ws.addEventListener("open", resolve, { once: true });
      this.ws.addEventListener("error", () => reject(new Error(`连接失败: ${this.url}`)), { once: true });
    });
    const hello = await this.call("hello", { token: this.token, client: "power-e2e/0.1" });
    if (hello.op !== "hello.ok") throw new Error(`握手被拒: ${JSON.stringify(hello.payload)}`);
  }
  call(op, payload, timeoutMs = 60000) {
    return new Promise((resolve, reject) => {
      const id = crypto.randomUUID();
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`超时: ${op}`));
      }, timeoutMs);
      this.pending.set(id, (env) => {
        clearTimeout(timer);
        env.op.endsWith(".err")
          ? reject(new Error(`${op} 失败: ${env.payload?.code} ${env.payload?.message}`))
          : resolve(env);
      });
      this.ws.send(
        JSON.stringify({ v: 1, id, op, ts: Date.now(), ...(payload === undefined ? {} : { payload }) }),
      );
    });
  }
  close() {
    this.ws?.close();
  }
}

const cfgDir = mkdtempSync(join(tmpdir(), "mixum-power-e2e-"));
const env = { ...process.env, MIXUM_CONFIG_DIR: cfgDir };

function ctl(args) {
  const r = spawnSync(CTL, args, { encoding: "utf8", env, timeout: 120000 });
  if (r.error) throw r.error;
  return r;
}

const up = containersUp();
if (!up.device || !up.cloud) {
  console.error("前置:先启动双容器(deviced/cloud),见文件头注释");
  process.exit(2);
}
const device = logsHave("mixum-power-device");
const cloud = logsHave("mixum-power-cloud");
expect(!!device.token && !!device.fingerprint, "device 容器就绪(token+指纹)");
expect(!!cloud.token && !!cloud.fingerprint, "cloud 容器就绪(token+指纹)");

// 0. 造真实内容:空 work_root 的快照 manifest 为 0 块,推送统计无从断言。
sh("docker", ["exec", "mixum-power-device", "sh", "-c",
  "mkdir -p /var/lib/mixum/work && echo 'mixum-power-e2e seed' > /var/lib/mixum/work/seed.txt"]);

// 1. 首次合盖迁移:快照+推送,CLI 指纹固定直连 device。
console.log("1. power --simulate-sleep(首次)");
const run1 = ctl([
  "power", "--simulate-sleep",
  "--addr", DEVICE,
  "--token", device.token,
  "--fingerprint", device.fingerprint,
  "--remote", REMOTE,
  "--remote-token", cloud.token,
]);
expect(run1.status === 0, `CLI 退出码 ${run1.status}`);
const node1 = run1.stdout.match(/合盖模拟完成: (\S+) \(推送 (\d+) 块 \/ (\d+) 字节\)/);
expect(!!node1, `首次迁移完成: ${run1.stdout.trim().split("\n").pop()}`);
expect(Number(node1[2]) >= 1, `首推 ${node1[2]} 块 / ${node1[3]} 字节`);
const firstId = node1[1];

// 2. 二次合盖迁移:新节点、内容未变 → 去重生效,零块重传。
console.log("2. power --simulate-sleep(二次)");
const run2 = ctl([
  "power", "--simulate-sleep",
  "--addr", DEVICE,
  "--token", device.token,
  "--fingerprint", device.fingerprint,
  "--remote", REMOTE,
  "--remote-token", cloud.token,
]);
expect(run2.status === 0, `CLI 退出码 ${run2.status}`);
const node2 = run2.stdout.match(/合盖模拟完成: (\S+) \(推送 (\d+) 块 \/ (\d+) 字节\)/);
expect(!!node2 && node2[1] !== firstId, `二次迁移新节点: ${node2?.[1]}`);
expect(Number(node2[2]) === 0, `重推零块(去重,新节点同内容)`);

// 3. 云端树:两个节点均已落地。
console.log("3. 云端树验证");
const cloudSession = new Session(CLOUD_HOST, cloud.token);
await cloudSession.connect();
try {
  const tree = await cloudSession.call("snap.tree", {});
  const ids = tree.payload.nodes.map((n) => n.id);
  expect(ids.includes(firstId), `云端树包含首次节点 ${firstId}`);
  expect(ids.includes(node2[1]), `云端树包含二次节点 ${node2[1]}`);
  const origin = tree.payload.nodes.find((n) => n.id === firstId).origin;
  expect(!!origin && origin.length > 0, `origin = ${origin}`);
} finally {
  cloudSession.close();
}

rmSync(cfgDir, { recursive: true, force: true });
console.log("power-e2e PASS");
