// text-input-v3 文本注入 e2e：宿主输入法提交的字符串 → 焦点窗口。
//
// 前置：镜像 mixumos-e2e-rust（或 MIXUM_E2E_IMAGE 指定的变体）内含
//       /usr/local/bin/ime-client（Dockerfile.e2e-rust 的 imeclient 阶段构建）。
// 链路：容器内起两个 ime-client 窗口（ime-a/ime-b）→ display.list 定位 →
//       注入指针把焦点切到 ime-a → display.input {type:"text"} →
//       ime-a 的 commit_string 收到文本，ime-b 收不到（焦点路由）；
//       再把焦点切到 ime-b 并验证超长文本被 bad_request 拒绝。
// 用法：node tests/integration/ime-e2e.mjs [port]
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawnSync } from "node:child_process";

const IMAGE = process.env.MIXUM_E2E_IMAGE ?? "mixumos-e2e-rust";
const NAME = "mixum-ime-e2e";
const PORT = Number(process.argv[2] ?? 17671);

function sh(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: "utf8" });
  if (r.error) throw r.error;
  return { out: (r.stdout ?? "") + (r.stderr ?? ""), code: r.status ?? 0 };
}

function expect(cond, message) {
  if (!cond) throw new Error(`断言失败: ${message}`);
  console.log(`  ok: ${message}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {}
    await sleep(300);
  }
  throw new Error(`超时: ${label}`);
}

class Client {
  constructor(url) {
    this.url = url;
    this.seq = 0;
    this.pending = new Map();
  }
  connect() {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;
      ws.addEventListener("open", () => resolve());
      ws.addEventListener("error", () => reject(new Error("WS 连接失败")));
      ws.addEventListener("message", (ev) => {
        const env = JSON.parse(String(ev.data));
        const cb = env.replyTo && this.pending.get(env.replyTo);
        if (cb) {
          this.pending.delete(env.replyTo);
          cb(env);
        }
      });
    });
  }
  call(op, payload) {
    return new Promise((resolve, reject) => {
      const id = crypto.randomUUID();
      const env = { v: 1, id, op, seq: ++this.seq, ts: Date.now(), payload };
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`超时: ${op}`));
      }, 15000);
      this.pending.set(id, (reply) => {
        clearTimeout(timer);
        if (reply.op.endsWith(".err")) {
          const err = new Error(reply.payload?.message ?? `${op} 失败`);
          err.code = reply.payload?.code;
          reject(err);
        } else {
          resolve(reply.payload);
        }
      });
      this.ws.send(JSON.stringify(env));
    });
  }
}

const readLog = (file) => sh("docker", ["exec", NAME, "cat", file]).out;

sh("docker", ["rm", "-f", NAME]);
sh("docker", ["run", "-d", "--rm", "--name", NAME, "-p", `${PORT}:7601`, IMAGE]);

const logs = await waitFor(async () => {
  const out = sh("docker", ["logs", NAME]).out;
  const m = out.match(/READY token=(\S+)/);
  return m ? m[1] : null;
}, 30000, "容器 READY");

sh("docker", [
  "exec", "-d",
  "-e", "XDG_RUNTIME_DIR=/run/mixum",
  "-e", "WAYLAND_DISPLAY=wayland-0",
  NAME, "sh", "-c", "ime-client ime-a > /tmp/ime-a.log 2>&1",
]);
sh("docker", [
  "exec", "-d",
  "-e", "XDG_RUNTIME_DIR=/run/mixum",
  "-e", "WAYLAND_DISPLAY=wayland-0",
  NAME, "sh", "-c", "ime-client ime-b > /tmp/ime-b.log 2>&1",
]);

try {
  const client = new Client(`wss://127.0.0.1:${PORT}/ws`);
  await client.connect();
  await client.call("hello", { token: logs, client: "ime-e2e/0.1", versions: ["1"] });

  const windows = await waitFor(async () => {
    const r = await client.call("display.list", {});
    const a = (r.windows ?? []).find((w) => w.title === "ime-a");
    const b = (r.windows ?? []).find((w) => w.title === "ime-b");
    return a && b ? { a, b } : null;
  }, 20000, "两个 ime-client 窗口");

  async function focus(id) {
    await client.call("display.input", { window: id, type: "pointer_motion", x: 20, y: 20 });
    await client.call("display.input", {
      window: id, type: "pointer_button", button: "left", action: "press",
    });
    await client.call("display.input", {
      window: id, type: "pointer_button", button: "left", action: "release",
    });
    await sleep(500);
  }

  await focus(windows.a.id);
  await client.call("display.input", {
    window: windows.a.id, type: "text", text: "中文A",
  });
  await sleep(800);
  const logA = readLog("/tmp/ime-a.log");
  const logB = readLog("/tmp/ime-b.log");
  expect(logA.includes("COMMIT:中文A"), "ime-a 收到 commit_string");
  expect(logA.includes("DONE:"), "ime-a 收到 done");
  expect(!logB.includes("COMMIT:"), "ime-b（未聚焦）未收到文本");

  await focus(windows.b.id);
  let rejected = false;
  try {
    await client.call("display.input", {
      window: windows.b.id, type: "text", text: "x".repeat(8193),
    });
  } catch (e) {
    rejected = e.code === "bad_request";
  }
  expect(rejected, "超长文本返回 bad_request");

  await client.call("display.input", {
    window: windows.b.id, type: "text", text: "中文B",
  });
  await sleep(800);
  const logB2 = readLog("/tmp/ime-b.log");
  expect(logB2.includes("COMMIT:中文B"), "ime-b 收到 commit_string");
  const logA2 = readLog("/tmp/ime-a.log");
  expect(!logA2.includes("中文B"), "焦点切换后 ime-a 不再收到文本");

  console.log("ime-e2e PASS");
} finally {
  sh("docker", ["rm", "-f", NAME]);
}
