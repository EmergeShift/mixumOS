// 拖拽上传 e2e:浏览器把文件拖到窗口画布 → file.put 分块 → 容器 transfers/。
//
// 前置:镜像 MIXUM_E2E_IMAGE(默认 mixumos-e2e-rust)可起 mixumd；
//       console/ 已 npm install；系统有 Edge/Chrome。
// 用法:node tests/integration/dragdrop-e2e.mjs [port] [devPort]
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const require2 = createRequire(new URL("../../console/package.json", import.meta.url));
const puppeteer = require2("puppeteer-core");
const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const IMAGE = process.env.MIXUM_E2E_IMAGE ?? "mixumos-e2e-rust";
const NAME = "mixum-dragdrop-e2e";
const PORT = Number(process.argv[2] ?? 17695);
const DEV_PORT = Number(process.argv[3] ?? 5299);

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
sh("docker", ["run", "-d", "--rm", "--name", NAME, "-p", `${PORT}:7601`, IMAGE]);

const token = await waitFor(() => {
  const m = sh("docker", ["logs", NAME]).out.match(/READY token=(\S+)/);
  return m ? m[1] : null;
}, 30000, "容器 READY");

const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const dev = spawn(npm, ["run", "dev", "--", "--port", String(DEV_PORT), "--strictPort"], {
  cwd: join(ROOT, "console"),
  env: { ...process.env, MIXUM_DEV_TARGET: `wss://127.0.0.1:${PORT}` },
  stdio: "ignore",
  shell: process.platform === "win32",
});

const DEV = `http://localhost:${DEV_PORT}/`;
let browser;
try {
  await waitFor(async () => (await fetch(DEV)).ok, 30000, "dev server 就绪");
  browser = await puppeteer.launch({
    executablePath,
    headless: "new",
    args: ["--no-sandbox", "--disable-gpu", "--window-size=1200,800"],
  });
  const page = await browser.newPage();
  await page.setViewport({ width: 1200, height: 800 });
  await page.goto(DEV, { waitUntil: "domcontentloaded" });
  await page.evaluate((t) => {
    localStorage.setItem("mixum.console.addr", "/ws");
    localStorage.setItem("mixum.console.token", t);
    localStorage.setItem("mixum.console.device", "console");
  }, token);
  await page.goto(DEV, { waitUntil: "domcontentloaded" });
  await page.waitForFunction(
    () => document.querySelector(".conn")?.textContent?.includes("已连接"),
    { timeout: 15000 },
  );
  await page.waitForSelector(".win-item", { timeout: 20000 });
  await page.click(".win-item");
  await page.waitForSelector(".canvas-panel canvas", { timeout: 10000 });
  await page.waitForFunction(
    () => document.querySelector(".canvas-panel canvas")?.width > 0,
    { timeout: 20000 },
  );

  const payload = `mixum-dragdrop-${Date.now()}`;
  expect(payload.length > 0, "准备测试文件");
  await page.evaluate((text) => {
    const file = new File([new TextEncoder().encode(text)], "drop-test.txt", {
      type: "text/plain",
    });
    const dt = new DataTransfer();
    dt.items.add(file);
    const wrap = document.querySelector(".canvas-wrap");
    wrap.dispatchEvent(
      new DragEvent("dragover", { bubbles: true, cancelable: true, dataTransfer: dt }),
    );
    wrap.dispatchEvent(
      new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer: dt }),
    );
  }, payload);
  await sleep(1500);

  const remote = sh("docker", [
    "exec", NAME, "sh", "-c", "cat /var/lib/mixum/work/transfers/drop-test.txt",
  ]).out.trim();
  expect(remote === payload, `容器收到文件内容（${remote.length} 字节）`);

  console.log("dragdrop-e2e PASS");
} finally {
  if (browser) await browser.close();
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(dev.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    dev.kill();
  }
  sh("docker", ["rm", "-f", NAME]);
}
