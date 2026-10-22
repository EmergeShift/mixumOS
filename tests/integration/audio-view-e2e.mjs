// 视图页音频 e2e：mixumctl serve → 页面 audio.attach → 0x02 帧 → 音量/静音。
//
// 前置：build/mixum-ctl-x64（当前代码 musl 构建）；镜像 MIXUM_E2E_IMAGE
//       （默认 mixumos-e2e-rust）可起 mixumd；系统有 Edge/Chrome。
// 用法：node tests/integration/audio-view-e2e.mjs [wsPort] [viewPort]
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const require2 = createRequire(new URL("../../console/package.json", import.meta.url));
const puppeteer = require2("puppeteer-core");
const CTL = join(ROOT, "build", "mixum-ctl-x64");
const IMAGE = process.env.MIXUM_E2E_IMAGE ?? "mixumos-e2e-rust";
const NAME = "mixum-audio-view-e2e";
const WS_PORT = Number(process.argv[2] ?? 17698);
const VIEW_PORT = Number(process.argv[3] ?? 17699);

if (!existsSync(CTL)) {
  console.error(`缺少 ${CTL}（cargo build --release --target x86_64-unknown-linux-musl -p mixum-ctl）`);
  process.exit(2);
}

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
    const v = await fn();
    if (v) return v;
    await sleep(300);
  }
  throw new Error(`超时: ${label}`);
}

const edges = [
  "C:/Program Files/Google/Chrome/Application/chrome.exe",
  "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
  "C:/Program Files/Microsoft/Edge/Application/msedge.exe",
];
const executablePath = edges.find((p) => existsSync(p));
if (!executablePath) {
  console.error("找不到 Chrome/Edge");
  process.exit(2);
}

sh("docker", ["rm", "-f", NAME]);
sh("docker", [
  "run", "-d", "--rm", "--name", NAME,
  "-p", `${WS_PORT}:7601`, "-p", `${VIEW_PORT}:7602`,
  "-e", "MIXUM_AUDIO_CMD=while :; do head -c 3840 /dev/urandom; sleep 0.02; done",
  IMAGE,
]);

let browser;
try {
  const { token, fingerprint } = await waitFor(() => {
    const out = sh("docker", ["logs", NAME]).out;
    const t = out.match(/READY token=(\S+)/)?.[1];
    const f = out.match(/fingerprint: (\S+)/)?.[1];
    return t && f ? { token: t, fingerprint: f } : null;
  }, 30000, "容器 READY");

  sh("docker", ["cp", CTL, `${NAME}:/usr/local/bin/mixumctl`]);
  sh("docker", ["exec", NAME, "chmod", "+x", "/usr/local/bin/mixumctl"]);
  sh("docker", [
    "exec", "-d", NAME, "sh", "-c",
    `mixumctl --addr wss://127.0.0.1:7601 --token ${token} --fingerprint ${fingerprint} serve --bind 0.0.0.0:7602 > /tmp/serve.log 2>&1`,
  ]);

  const win = await waitFor(async () => {
    try {
      const r = await fetch(`http://127.0.0.1:${VIEW_PORT}/api/windows`);
      if (!r.ok) return null;
      return (await r.json()).windows?.[0] ?? null;
    } catch {
      return null;
    }
  }, 20000, "视图服务/窗口");

  browser = await puppeteer.launch({
    executablePath,
    headless: "new",
    args: ["--no-sandbox", "--disable-gpu", "--autoplay-policy=no-user-gesture-required"],
  });
  const page = await browser.newPage();
  await page.setViewport({ width: 520, height: 420 });
  await page.goto(`http://127.0.0.1:${VIEW_PORT}/view/${win.id}`, {
    waitUntil: "domcontentloaded",
  });
  await page.waitForFunction(
    () => document.querySelector("canvas")?.width > 0,
    { timeout: 15000 },
  );

  await page.click("#au");
  await page.waitForFunction(
    () => (document.querySelector("#af")?.textContent ?? "").length > 0,
    { timeout: 10000 },
  );
  const badge = await page.$eval("#af", (el) => el.textContent);
  expect(Number.parseInt(badge, 10) >= 25, `音频帧计数 ${badge}`);
  expect((await page.$eval("#au", (el) => el.textContent)) === "静音", "按钮切换为静音");

  await page.evaluate(() => {
    const v = document.querySelector("#vol");
    v.value = 30;
    v.dispatchEvent(new Event("input"));
  });
  expect((await page.$eval("#vol", (el) => el.value)) === "30", "音量滑杆可调");
  await page.evaluate(() => {
    const v = document.querySelector("#off");
    v.value = 40;
    v.dispatchEvent(new Event("input"));
  });
  expect((await page.$eval("#off", (el) => el.value)) === "40", "A/V 偏移滑杆可调");

  await page.click("#au");
  await sleep(300);
  expect((await page.$eval("#af", (el) => el.textContent)) === "", "静音后计数清空");

  console.log("audio-view-e2e PASS");
} catch (e) {
  console.log("--- serve log ---");
  console.log(sh("docker", ["exec", NAME, "cat", "/tmp/serve.log"]));
  throw e;
} finally {
  if (browser) await browser.close();
  sh("docker", ["rm", "-f", NAME]);
}
