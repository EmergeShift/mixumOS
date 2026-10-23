// 剪贴板 e2e:console 会话 ↔ 容器 Wayland 选区双向同步。
// 下行 display.clipboard.set → 容器 wl-paste 读到;
// 上行容器 wl-copy → display.clipboard.changed 事件推到会话;
// 归属:agent 持有窗口时下行被拒(与 input 同规);空串拒绝。
// 用法: node tests/integration/clipboard-e2e.mjs <token> [port] [container]
import { execFileSync } from "node:child_process";
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
const token = process.argv[2];
const port = process.argv[3] ?? 17601;
const container = process.argv[4] ?? "mixum-clip";
if (!token) { console.error("用法: node clipboard-e2e.mjs <token> [port] [container]"); process.exit(2); }
const URL_ = `wss://127.0.0.1:${port}/ws`;

function sh(cmd) {
  // docker exec 不继承 entrypoint 的 env，手动补 Wayland 环境。
  return execFileSync("docker", ["exec", container, "sh", "-c",
    `export XDG_RUNTIME_DIR=/run/mixum WAYLAND_DISPLAY=wayland-0; ${cmd}`],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).replace(/\r?\n$/, "");
}

// 拉起第二个 GUI 窗口，用于验证上行文本的窗口归属。
function spawnFoot() {
  execFileSync("docker", ["exec", "-d", container, "sh", "-c",
    "export XDG_RUNTIME_DIR=/run/mixum WAYLAND_DISPLAY=wayland-0; " +
    "exec foot --override=main.initial-window-size-pixels=300x200 --hold cat /etc/alpine-release"],
    { stdio: "ignore" });
}

function frame(op, payload) {
  return JSON.stringify({ v: 1, id: crypto.randomUUID(), op, ts: Date.now(), ...(payload ? { payload } : {}) });
}

const s = {
  ws: null,
  pending: new Map(),
  clips: [],
  async connect() {
    this.ws = new WebSocket(URL_);
    this.ws.addEventListener("message", (ev) => {
      if (typeof ev.data !== "string") return;
      const env = JSON.parse(ev.data);
      const w = this.pending.get(env.replyTo);
      if (w) { this.pending.delete(env.replyTo); w(env); return; }
      if (env.op === "display.clipboard.changed") {
        this.clips.push({ window: env.payload?.window, text: env.payload?.text ?? "" });
      }
    });
    await new Promise((res, rej) => {
      this.ws.addEventListener("open", res, { once: true });
      this.ws.addEventListener("error", () => rej(new Error("连接失败")), { once: true });
    });
    this.call = this.call.bind(this);
    const h = await this.call("hello", { token, client: "clipboard-e2e/0.1" });
    if (!h) throw new Error("握手失败");
  },
  call(op, payload) {
    return new Promise((res, rej) => {
      const raw = frame(op, payload);
      const id = JSON.parse(raw).id;
      const t = setTimeout(() => rej(new Error(`超时 ${op}`)), 15000);
      this.pending.set(id, (env) => {
        clearTimeout(t);
        env.op.endsWith(".err") ? rej(new Error(env.payload?.message)) : res(env.payload);
      });
      this.ws.send(raw);
    });
  },
  close() { try { this.ws.close(); } catch { /* noop */ } },
};

await s.connect();
await s.call("display.windows", {}); // 同时开通剪贴板推送
const list = await s.call("display.list", {});
const win = list.windows[0].id;

// 1. 下行:console → 容器选区 → wl-paste 可读。
const down = `mixum-clip-down-${crypto.randomUUID().slice(0, 8)}`;
await s.call("display.clipboard.set", { window: win, text: down });
await new Promise((r) => setTimeout(r, 1200));
const pasted = sh("wl-paste --no-newline 2>&1 || wl-paste 2>&1");
console.log(`下行:发送 ${JSON.stringify(down)} → wl-paste ${JSON.stringify(pasted)}`);

let emptyRejected = false;
try {
  await s.call("display.clipboard.set", { window: win, text: "" });
} catch {
  emptyRejected = true;
}
console.log(`空串拒绝:${emptyRejected ? "是" : "否(异常)"}`);

let oversizedRejected = false;
try {
  await s.call("display.clipboard.set", {
    window: win,
    text: "x".repeat(4 * 1024 * 1024 + 1),
  });
} catch {
  oversizedRejected = true;
}
console.log(`4MiB+1 拒绝:${oversizedRejected ? "是" : "否(异常)"}`);

// 控制字符 3MiB:JSON 转义后 18MiB,超过 comp 控制帧上限,必须拒绝且不断链。
let escapedRejected = false;
try {
  await s.call("display.clipboard.set", {
    window: win,
    text: "\u0001".repeat(3 * 1024 * 1024),
  });
} catch {
  escapedRejected = true;
}
const afterEscape = await s.call("display.list", {});
console.log(
  `转义超限拒绝:${escapedRejected ? "是" : "否(异常)"},连接存活:${Array.isArray(afterEscape.windows) ? "是" : "否"}`,
);

// 2. 上行:容器 wl-copy → display.clipboard 推送。
const up = `mixum-clip-up-${crypto.randomUUID().slice(0, 8)}`;
sh(`printf %s "${up}" | wl-copy`);
let received = null;
for (let i = 0; i < 50 && !received; i++) {
  await new Promise((r) => setTimeout(r, 100));
  received = s.clips.find((item) => item.text === up) ?? null;
}
console.log(`上行:wl-copy ${JSON.stringify(up)} → 事件 ${s.clips.length} 条${received ? `,命中 ${JSON.stringify(received)}` : ""}`);

// 2b. 多窗口归属：注入焦点后再复制，事件必须带对应窗口 id。
spawnFoot();
let windows = [];
for (let i = 0; i < 50 && windows.length < 2; i++) {
  await new Promise((r) => setTimeout(r, 100));
  windows = (await s.call("display.list", {})).windows ?? [];
}
const winA = windows[0]?.id;
const winB = windows[1]?.id;
const route = async (windowId) => {
  const text = `mixum-clip-route-${windowId}-${crypto.randomUUID().slice(0, 8)}`;
  await s.call("display.input", { window: windowId, type: "pointer_motion", x: 5, y: 5 });
  // comp 的控制通道 200ms 轮询一次:等它处理完焦点注入再触发复制。
  await new Promise((r) => setTimeout(r, 300));
  sh(`printf %s "${text}" | wl-copy`);
  for (let i = 0; i < 50; i++) {
    await new Promise((r) => setTimeout(r, 100));
    const hit = s.clips.find((item) => item.text === text) ?? null;
    if (hit) return hit;
  }
  return null;
};
const routedA = winA != null ? await route(winA) : null;
const routedB = winB != null ? await route(winB) : null;
console.log(
  `多窗口归属:A=${winA}→${routedA?.window ?? "无事件"},B=${winB}→${routedB?.window ?? "无事件"}`,
);

// 3. 归属:用户接管(claim)后与 input 同规放行;agent 持有拒绝由
//    display.rs 单测覆盖(容器内无运行中任务,不重复构造)。
await s.call("display.takeover", { window: win, action: "claim", force: true });
let userOk = false;
try {
  await s.call("display.clipboard.set", { window: win, text: "user-owned-sync" });
  userOk = true;
} catch { /* 拒绝即失败 */ }
await s.call("display.takeover", { window: win, action: "release" });
console.log(`归属:user 接管下下行${userOk ? "放行(与 input 同规)" : "被拒(异常)"}`);

// 4. agent 模式:任务活跃时窗口归 agent,下行必须拒绝。
const task = await s.call("task.submit", {
  instruction: "sleep 30",
  agent: "shell",
  runner: "here",
  params: { cmd: "sleep 30" },
});
let agentRejected = false;
for (let i = 0; i < 30; i++) {
  const current = (await s.call("display.list", {})).windows?.find((item) => item.id === win);
  if (current?.mode === "agent") break;
  await new Promise((r) => setTimeout(r, 100));
}
try {
  await s.call("display.clipboard.set", { window: win, text: "agent-owned" });
} catch {
  agentRejected = true;
}
if (task?.id) await s.call("task.cancel", { id: task.id });
console.log(`agent 模式拒绝:${agentRejected ? "是" : "否(异常)"}`);

// 4MiB 边界放在最后:4MB 控制消息会占用 comp 的控制通道,避免影响上面的
// 焦点注入/上行归属用例。
let maxAccepted = false;
try {
  await s.call("display.clipboard.set", { window: win, text: "y".repeat(4 * 1024 * 1024) });
  maxAccepted = true;
} catch { /* 拒绝即失败 */ }
console.log(`4MiB 边界接受:${maxAccepted ? "是" : "否(异常)"}`);
s.close();

const ok =
  pasted === down &&
  received?.text === up &&
  received?.window === win &&
  emptyRejected &&
  oversizedRejected &&
  escapedRejected &&
  Array.isArray(afterEscape.windows) &&
  maxAccepted &&
  routedA?.window === winA &&
  routedB?.window === winB &&
  agentRejected &&
  userOk;
console.log(ok ? "PASS 剪贴板:下行入选区/上行出事件/归属与 ACL/边界与多窗口" : "FAIL 剪贴板双向同步不符");
process.exit(ok ? 0 : 1);
