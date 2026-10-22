// 本地视图页真浏览器验证(puppeteer-core + 系统 Edge):
// 打开 /view/1,断言 canvas 收到帧(尺寸 300x160)且渲染非均匀像素,截图存证。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
import { createRequire } from "node:module";
import { join } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const require2 = createRequire(join(ROOT, "console", "package.json"));
const puppeteer = require2("puppeteer-core");
const CTL =
  process.platform === "win32"
    ? join(ROOT, "rust", "target", "debug", "mixum-ctl.exe")
    : join(ROOT, "rust", "target", "debug", "mixum-ctl");

function sh(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: "utf8" });
  if (r.error) throw r.error;
  return (r.stdout ?? "") + (r.stderr ?? "");
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logs = sh("docker", ["logs", "mixum-view"]);
const token = logs.match(/READY token=(\S+)/)?.[1];
const fingerprint = logs.match(/fingerprint: (\S+)/)?.[1];
if (!token || !fingerprint) {
  console.error("前置: mixum-view 容器须已就绪(见 view-e2e.mjs)");
  process.exit(2);
}

const serve = spawn(
  CTL,
  ["serve", "--bind", "127.0.0.1:17623",
   "--addr", "wss://127.0.0.1:17621",
   "--token", token, "--fingerprint", fingerprint],
  { env: { ...process.env, MIXUM_CONFIG_DIR: join(ROOT, "build", "view-browser-dbg") }, stdio: "ignore" },
);
await sleep(3000);

const edges = [
  "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
  "C:/Program Files/Microsoft/Edge/Application/msedge.exe",
];
const executablePath = edges.find((p) => spawnSync("ls", [p]).status === 0);

const browser = await puppeteer.launch({ executablePath, headless: true });
try {
  const page = await browser.newPage();
  await page.setViewport({ width: 520, height: 420 });
  await page.goto("http://127.0.0.1:17623/view/1", { waitUntil: "domcontentloaded" });
  await page.waitForFunction(() => document.querySelector("canvas")?.width > 0, { timeout: 15000 });
  await sleep(1500);
  const info = await page.evaluate(() => {
    const c = document.querySelector("canvas");
    const x = c.getContext("2d");
    const d = x.getImageData(0, 0, c.width, c.height).data;
    const seen = new Set();
    for (let i = 0; i < d.length; i += 403 * 4) seen.add(`${d[i]},${d[i + 1]},${d[i + 2]}`);
    return {
      w: c.width,
      h: c.height,
      uniqueColors: seen.size,
      title: document.title,
      bar: document.querySelector("#bar")?.textContent,
    };
  });
  console.log(`canvas ${info.w}x${info.h}, 独立颜色 ${info.uniqueColors}, 标题「${info.title}」, 栏「${info.bar}」`);
  if (info.w !== 300 || info.h !== 160) throw new Error("canvas 尺寸与窗口不符");
  if (info.uniqueColors < 3) throw new Error("画面疑似未渲染(颜色过于均匀)");

  function expect(cond, message) {
    if (!cond) throw new Error(`断言失败: ${message}`);
    console.log(`  ok: ${message}`);
  }
  const modeOnServer = async () =>
    (await (await fetch("http://127.0.0.1:17623/api/windows")).json()).windows[0].mode;

  // 1. 接管:badge 变 user,服务端模式同步。
  expect((await modeOnServer()) === "free", "初始模式 free");
  await page.click("#tk");
  await page.waitForFunction(
    () => document.querySelector("#mode")?.textContent === "user",
    { timeout: 10000 },
  );
  expect((await modeOnServer()) === "user", "接管后服务端模式 user");
  expect((await page.$eval("#tk", (b) => b.textContent)) === "释放", "按钮变「释放」");

  // 2. 输入注入:测试自己另开一条帧流 WS 计数,指针移动应引出 damage 新帧。
  const frames = await new Promise((resolve, reject) => {
    const ws = new WebSocket("ws://127.0.0.1:17623/api/stream/1");
    ws.binaryType = "arraybuffer";
    let count = 0;
    ws.onmessage = (ev) => {
      if (new Uint8Array(ev.data)[0] !== 1) return;
      count += 1;
      if (count === 1) resolve({ ws, next: () => count });
    };
    setTimeout(() => reject(new Error("超时:测试帧流未出首帧")), 15000);
  });
  const baseline = frames.next();
  const deadline = Date.now() + 8000;
  let moved = 0;
  while (Date.now() < deadline) {
    await page.mouse.move(60 + moved, 80 + (moved % 20));
    moved += 7;
    await sleep(150);
    if (frames.next() > baseline) break;
  }
  expect(frames.next() > baseline, `指针移动引出 damage 新帧(${frames.next()} > ${baseline})`);
  frames.ws.close();

  // 3. 键盘注入不炸(foot 无回显,仅验证通路无异常+模式保持)。
  await page.focus("#c");
  await page.keyboard.press("a");
  await sleep(500);
  expect((await modeOnServer()) === "user", "键盘注入后模式保持 user");

  // 4. 释放:badge 回 free。
  await page.click("#tk");
  await page.waitForFunction(
    () => document.querySelector("#mode")?.textContent === "free",
    { timeout: 10000 },
  );
  expect((await modeOnServer()) === "free", "释放后服务端模式 free");

  await page.screenshot({ path: join(ROOT, "build", "view-browser.png") });
  console.log("view-browser PASS");
} finally {
  await browser.close();
  serve.kill();
  await sleep(300);
}
