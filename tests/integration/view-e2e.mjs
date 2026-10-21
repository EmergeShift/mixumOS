// mixum-ctl 本地窗口视图服务 e2e:真实 serve 二进制 + 容器 GUI 栈。
//
// 前置:镜像 mixumos-e2e-rust(默认 entrypoint 起 mixumd+mixcomp+foot);
//       cargo build -p mixum-ctl(宿主)。
// 链路:serve --bind 127.0.0.1:17622 ← 容器(发布 17621);
//       GET /api/windows 列出 foot 窗口 → /view/<id> 页面 →
//       WS /api/stream/<id> 收到 PNG/JPEG 帧(§5 帧头解析+魔数校验)。
// 用法: node tests/integration/view-e2e.mjs [apiPort] [viewPort]
process.env.MSYS_NO_PATHCONV = "1";
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const API_PORT = process.argv[2] ?? 17621;
const VIEW_PORT = process.argv[3] ?? 17622;
const API = `http://127.0.0.1:${VIEW_PORT}`;
const CTL =
  process.platform === "win32"
    ? join(ROOT, "rust", "target", "debug", "mixum-ctl.exe")
    : join(ROOT, "rust", "target", "debug", "mixum-ctl");

function sh(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: "utf8" });
  if (r.error) throw r.error;
  return (r.stdout ?? "") + (r.stderr ?? "");
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
    await sleep(500);
  }
  throw new Error(`超时: ${label}`);
}

// 容器就绪:entrypoint 打印 READY token=,等 GUI 栈起来。
const logs = await waitFor(async () => {
  const out = sh("docker", ["logs", "mixum-view"]);
  const m = out.match(/READY token=(\S+)/);
  const fp = out.match(/fingerprint: (\S+)/)?.[1];
  return m && fp ? { token: m[1], fingerprint: fp } : null;
}, 30000, "容器 READY");

const cfgDir = mkdtempSync(join(tmpdir(), "mixum-view-e2e-"));
const serve = spawn(
  CTL,
  ["serve", "--bind", `127.0.0.1:${VIEW_PORT}`,
   "--addr", `wss://127.0.0.1:${API_PORT}`,
   "--token", logs.token,
   "--fingerprint", logs.fingerprint],
  { env: { ...process.env, MIXUM_CONFIG_DIR: cfgDir }, stdio: ["ignore", "pipe", "pipe"] },
);
let serveLog = "";
serve.stdout.on("data", (d) => (serveLog += d));
serve.stderr.on("data", (d) => (serveLog += d));
serve.on("error", (e) => (serveLog += `\nspawn error: ${e}`));
try {
  try {
    await waitFor(async () => (await fetch(`${API}/api/windows`)).ok, 15000, "serve 就绪");
  } catch (e) {
    throw new Error(`${e.message}\nserve 日志: ${serveLog || "(空)"}`);
  }

  // 1. 管理页与路由。
  const index = await fetch(`${API}/`);
  expect(index.ok, "GET / → 200");
  expect((await index.text()).includes("mixum 窗口"), "管理页 HTML");

  // 2. 窗口列表:foot 窗口就位。
  const list = await (await fetch(`${API}/api/windows`)).json();
  const windows = list.windows || [];
  expect(windows.length >= 1, `窗口列表 ${windows.length} 个`);
  const foot = windows[0];
  expect(Number.isInteger(foot.id), `窗口 id=${foot.id} title=${foot.title || foot.appId || ""}`);

  // 3. 视图页:嵌入窗口 id。
  const view = await fetch(`${API}/view/${foot.id}`);
  expect(view.ok, `GET /view/${foot.id} → 200`);
  expect((await view.text()).includes(`id=${foot.id},`), "视图页嵌入窗口 id");

  // 4. 未知路径 404。
  const missing = await fetch(`${API}/nope`);
  expect(missing.status === 404, `GET /nope → ${missing.status}`);

  // 5. WS 帧流:§5 帧头解析 + JPEG 魔数。
  const frame = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("超时:未收到帧")), 15000);
    const ws = new WebSocket(`ws://127.0.0.1:${VIEW_PORT}/api/stream/${foot.id}`);
    ws.binaryType = "arraybuffer";
    ws.onmessage = (ev) => {
      const b = new Uint8Array(ev.data), dv = new DataView(ev.data);
      if (b[0] !== 1) return;
      const w = dv.getUint32(5, true), h = dv.getUint32(9, true), n = dv.getUint32(13, true);
      clearTimeout(timer);
      ws.close();
      resolve({ w, h, n, magic: [b[27], b[28]] });
    };
    ws.onerror = () => { clearTimeout(timer); reject(new Error("WS 错误")); };
  });
  expect(frame.w > 0 && frame.h > 0, `帧尺寸 ${frame.w}x${frame.h}`);
  expect(frame.n > 0, `帧载荷 ${frame.n} 字节`);
  const png = frame.magic[0] === 0x89 && frame.magic[1] === 0x50;
  const jpeg = frame.magic[0] === 0xff && frame.magic[1] === 0xd8;
  expect(png || jpeg, `PNG/JPEG 魔数 (${frame.magic.map((v) => v.toString(16)).join(" ")})`);

  console.log("view-e2e PASS");
} finally {
  serve.kill();
  await sleep(300);
  spawnSync("docker", ["rm", "-f", "mixum-view"], { stdio: "ignore" });
  rmSync(cfgDir, { recursive: true, force: true });
  if (serveLog && !serveLog.includes("本地视图")) console.error(serveLog);
}
