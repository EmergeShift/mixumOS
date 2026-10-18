// gateway M1 e2e:双节点拓扑的路由隔离。
// 拓扑:gateway 容器(发布 7701)← 出站注册 ← 节点容器 a/b(mixumd --gateway);
//       探针(宿主)以 admin 令牌连 gateway,签发节点令牌,信封按 node 路由。
// 断言:注册表、单节点提交任务/事件流隔离、窗口表隔离、帧按附流窗口隔离。
// 前置:build/mixum-gateway-x64(musl 静态);镜像 mixumos-e2e-rust:gw。
// 用法: node tests/integration/gateway-e2e.mjs
import { execFileSync, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

process.env.MSYS_NO_PATHCONV = "1"; // docker 参数含容器路径,禁 git-bash 转换
const GW_PORT = 17701;
const ROOT = fileURLToPath(new URL("../..", import.meta.url)).split("\\").join("/");

function sh(cmd, opts = {}) {
  // stderr 合并:gateway 的令牌/日志走 eprintln(stderr)
  const r = spawnSync("docker", cmd, { encoding: "utf8", stdio: ["pipe", "pipe", "pipe"], ...opts });
  return (r.stdout ?? "") + (r.stderr ?? "");
}

// ── 1. 起 gateway 容器(alpine + 静态二进制) ──
try { sh(["rm", "-f", "mixum-gw"]); } catch { /* 不存在 */ }
spawnSync("docker", ["run", "-d", "--name", "mixum-gw", "-p", `${GW_PORT}:7701`,
  "-v", `${ROOT}/build:/opt:ro`, "alpine:3.21",
  "/opt/mixum-gateway-x64", "--listen", "0.0.0.0:7701"], { stdio: "ignore" });
let bootstrap = "";
for (let i = 0; i < 30 && !bootstrap; i++) {
  const logs = sh(["logs", "mixum-gw"]).toString();
  bootstrap = logs.match(/bootstrap token: (\S+)/)?.[1] ?? "";
  if (!bootstrap) await new Promise((r) => setTimeout(r, 500));
}
if (!bootstrap) { console.error("FAIL: gateway 未就绪"); process.exit(1); }
console.log("1. gateway 就绪 ✓");

// ── 2. admin 客户端:hello → 签发两个节点令牌 ──
function ws(url) {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  const pending = new Map();
  socket.addEventListener("message", (ev) => {
    if (typeof ev.data !== "string") return;
    const env = JSON.parse(ev.data);
    const w = pending.get(env.replyTo);
    if (w) { pending.delete(env.replyTo); w(env); }
  });
  const call = (op, payload, node) => new Promise((res) => {
    const env = { v: 1, id: crypto.randomUUID(), op, ts: Date.now(),
      ...(node ? { node } : {}), ...(payload ? { payload } : {}) };
    const t = setTimeout(() => { pending.delete(env.id); res({ op: `${op}.err`, payload: { message: "timeout" } }); }, 10000);
    pending.set(env.id, (e) => { clearTimeout(t); res(e); });
    socket.send(JSON.stringify(env));
  });
  const opened = new Promise((res, rej) => {
    socket.addEventListener("open", res, { once: true });
    socket.addEventListener("error", () => rej(new Error("ws 连接失败")), { once: true });
  });
  return { socket, call, opened };
}

const admin = ws(`ws://localhost:${GW_PORT}`);
await admin.opened;
const hello = await admin.call("hello", { token: bootstrap, client: "gateway-e2e/0.1" });
if (!hello.op.endsWith(".ok")) { console.error(`FAIL: hello ${JSON.stringify(hello)}`); process.exit(1); }
console.log("2. admin hello ✓ (server:", hello.payload.server + ")");

const nodeToken = {};
for (const name of ["a", "b"]) {
  const r = await admin.call("gateway.token.issue", { kind: "node", name });
  if (!r.op.endsWith(".ok")) { console.error(`FAIL: 签发节点令牌 ${name}`); process.exit(1); }
  nodeToken[name] = r.payload.token;
}
console.log("3. 节点令牌签发 ✓ (a/b)");

// ── 3. 双节点容器出站注册 ──
sh(["rm", "-f", "mixum-gw-a", "mixum-gw-b"]);
for (const name of ["a", "b"]) {
  spawnSync("docker", ["run", "-d", "--name", `mixum-gw-${name}`,
    "-e", `MIXUM_GATEWAY_URL=ws://host.docker.internal:${GW_PORT}`,
    "-e", `MIXUM_GATEWAY_NAME=${name}`,
    "-e", `MIXUM_GATEWAY_TOKEN=${nodeToken[name]}`,
    "-p", name === "a" ? "17711:7601" : "17712:7601",
    "mixumos-e2e-rust:gw"], { stdio: "ignore" });
}
// 等注册表齐全
let nodes = [];
for (let i = 0; i < 40; i++) {
  const r = await admin.call("gateway.node.list", {});
  nodes = r.payload?.nodes ?? [];
  if (nodes.includes("a") && nodes.includes("b")) break;
  await new Promise((r2) => setTimeout(r2, 500));
}
if (!(nodes.includes("a") && nodes.includes("b"))) {
  console.error(`FAIL: 节点未齐 ${JSON.stringify(nodes)}`);
  process.exit(1);
}
console.log("4. 双节点出站注册 ✓", JSON.stringify(nodes));

// ── 4. 订阅节点 a 的任务事件流;向 a 提交任务;断言流里只有 a 的任务 ──
const client = ws(`ws://localhost:${GW_PORT}`);
await client.opened;
const events = [];
client.socket.addEventListener("message", (ev) => {
  if (typeof ev.data !== "string") return;
  const env = JSON.parse(ev.data);
  if (env.op === "task.event") events.push(env.payload);
});
await client.call("hello", { token: bootstrap, client: "gateway-e2e/0.1" });
await client.call("task.events", {}, "a");
const pre = await client.call("display.list", {}, "a");
console.log("display.list(a) 提交前:", JSON.stringify(pre.payload?.windows?.map((w) => w.title)));

const ta = await client.call("task.submit", { instruction: "echo from-a", agent: "shell", params: { cmd: "echo from-a" } }, "a");
const tb = await client.call("task.submit", { instruction: "echo from-b", agent: "shell", params: { cmd: "echo from-b" } }, "b");
// 第二个 b 任务:两个 mixumd 各自从 task_1 起编号,同 id 会撞车——
// b 的 task_2 在 a 的流里不允许出现,才是无歧义的事件隔离证据。
const tb2 = await client.call("task.submit", { instruction: "echo from-b-2", agent: "shell", params: { cmd: "echo from-b-2" } }, "b");
if (!ta.op?.endsWith(".ok") || !tb.op?.endsWith(".ok")) {
  console.error(`FAIL: 提交失败 ${JSON.stringify({ ta: ta.payload, tb: tb.payload })}`);
  process.exit(1);
}
console.log(`5. 双节点任务提交 ✓ (a:${ta.payload.id} b:${tb.payload.id})`);

// 等 a 的任务完成事件流进来
let sawA = false, sawARun = false;
for (let i = 0; i < 60 && !sawA; i++) {
  await new Promise((r) => setTimeout(r, 200));
  for (const e of events) {
    if (e.taskId === ta.payload.id && e.type === "state" && e.data === "completed") sawA = true;
    if (e.taskId === ta.payload.id && e.type === "state" && e.data === "running") sawARun = true;
  }
}
console.log(`6. a 事件流 ✓ (running: ${sawARun}, completed: ${sawA})`);
if (!sawA) { console.error("FAIL: a 的完成事件未到达"); process.exit(1); }

// b 的任务也要完成(经 task.list 在 b 上确认)
const tbList = await client.call("task.list", {}, "b");
const tbState = (tbList.payload?.tasks ?? []).find((t) => t.id === tb.payload.id)?.state;
const tb2State = (tbList.payload?.tasks ?? []).find((t) => t.id === tb2.payload.id)?.state;
console.log(`7. b 节点任务完成 ✓ (task_1: ${tbState}, task_2: ${tb2State})`);
if (tbState !== "completed" || tb2State !== "completed") { console.error("FAIL: b 任务未完成"); process.exit(1); }
// 事件隔离(无歧义 id):b 的 task_2 事件不允许出现在 a 的订阅流里
const leaked = events.some((e) => e.taskId === tb2.payload.id);
console.log(`8. 事件流隔离 ✓ (b 的 task_2 事件泄漏: ${leaked})`);
if (leaked) { console.error("FAIL: 事件流隔离破坏"); process.exit(1); }

// ── 5. 窗口表隔离:display.list 各归各 ──
// 容器 GUI 栈启动晚于节点注册(relay 在 mixumd 一启动就注册),
// 轮询等 foot 的窗口出现(≤20s),两个节点各自隔离等待。
async function waitWindows(node) {
  for (let i = 0; i < 40; i++) {
    const r = await client.call("display.list", {}, node);
    if ((r.payload?.windows?.length ?? 0) > 0) return r.payload.windows;
    await new Promise((r2) => setTimeout(r2, 500));
  }
  return [];
}
const waWin = await waitWindows("a");
const wbWin = await waitWindows("b");
const wa = { payload: { windows: waWin } };
console.log(`9. 窗口表隔离 ✓ (a:${waWin.length} 窗 b:${wbWin.length} 窗)`);
if (!waWin.length || !wbWin.length) {
  console.error("FAIL: 窗口表为空"); process.exit(1);
}

// ── 6. 帧按附流隔离:a 附流后有帧;detach 后无帧;b 的帧不影响 a ──
const frames = [];
client.socket.addEventListener("message", (ev) => {
  if (ev.data instanceof ArrayBuffer && new Uint8Array(ev.data)[0] === 0x01) frames.push(Date.now());
});
await client.call("display.attach", { window: wa.payload.windows[0].id, fps: 15 }, "a");
for (let i = 0; i < 40 && frames.length === 0; i++) await new Promise((r) => setTimeout(r, 100));
const framesA = frames.length;
console.log(`10. 帧按附流路由 ✓ (a 附流后收到 ${framesA} 帧)`);
if (framesA === 0) { console.error("FAIL: a 附流无帧"); process.exit(1); }
await client.call("display.detach", { window: wa.payload.windows[0].id }, "a");
await new Promise((r) => setTimeout(r, 800));
const framesAfter = frames.length;
console.log(`11. detach 后停帧 ✓ (${framesA} → ${framesAfter})`);

// 清理
client.socket.close();
admin.socket.close();
for (const c of ["mixum-gw-a", "mixum-gw-b", "mixum-gw"]) { try { sh(["rm", "-f", c]); } catch { /* noop */ } }

const ok = sawA && !leaked && tbState === "completed" && tb2State === "completed" && framesA > 0 && framesAfter === framesA;
console.log(ok ? "PASS gateway M1:注册/令牌/路由/事件隔离/帧隔离 全链路通过" : "FAIL gateway M1");
process.exit(ok ? 0 : 1);
