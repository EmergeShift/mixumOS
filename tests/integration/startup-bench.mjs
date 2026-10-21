// mixumOS 启动延迟基准:容器冷启动/热重启到 mixumd 可连接(listening)。
//
// 指标(均为 wall clock,毫秒):
//   containerToReady = listening 日志时间戳 - 容器 State.StartedAt
//                      (容器内真实启动延迟,不含 Docker 调度/创建)
//   runToReady       = listening 日志时间戳 - 宿主发起 docker run/start 的时刻
//                      (端到端,含 CLI/daemon/创建开销)
// 时间戳取自 docker logs -t(宿主系统时钟),不受轮询间隔影响。
//
// 用法: node tests/integration/startup-bench.mjs [--image <ref>] [--runs N] [--mode cold|warm|both]
// 输出: 控制台表格 + build/startup-bench.json(gitignored)
process.env.MSYS_NO_PATHCONV = "1";

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const arg = (name, fallback) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : fallback;
};
const IMAGE = arg("--image", "mixumos:0.1.0-x86_64");
const RUNS = Number(arg("--runs", "10"));
const MODE = arg("--mode", "both");
const NAME = "mixum-startup-bench";
const ENTRY = ["--entrypoint", "/usr/bin/mixumd", IMAGE, "--config", "/dev/null"];
const READY = "mixum-agentd listening on";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function docker(args) {
  const r = spawnSync("docker", args, { encoding: "utf8", env: process.env });
  if (r.error) throw r.error;
  return r;
}
function must(args) {
  const r = docker(args);
  if (r.status !== 0) {
    throw new Error(`docker ${args.join(" ")} 失败: ${(r.stderr || r.stdout).trim()}`);
  }
  return r.stdout ?? "";
}
function ts(s) {
  // docker 时间戳: 2026-10-21T08:30:00.123456789Z(纳秒截断到毫秒足够)
  return Date.parse(s);
}
function logLines(name) {
  // mixumd 日志走容器 stderr,docker CLI 会把它转到自己的 stderr,必须合并。
  const r = docker(["logs", "-t", name]);
  if (r.status !== 0) throw new Error(`docker logs 失败: ${(r.stderr || "").trim()}`);
  const text = `${r.stdout ?? ""}${r.stderr ?? ""}`;
  // 两路输出按行收集后按 ISO 时间戳排序,保证跨流顺序稳定。
  return text.split(/\r?\n/).filter(Boolean).sort();
}
async function waitListening(name, skip = 0, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const lines = logLines(name);
    const hit = lines.slice(skip).find((line) => line.includes(READY));
    if (hit) {
      const stamp = hit.match(/^(\S+)\s/)[1];
      return { readyTs: ts(stamp), lines: lines.length };
    }
    await sleep(30);
  }
  throw new Error(`超时: ${name} 未出现 listening`);
}
function startedAt(name) {
  return ts(must(["inspect", "--format", "{{.State.StartedAt}}", name]).trim());
}
function stats(values) {
  const v = [...values].sort((a, b) => a - b);
  const mean = v.reduce((a, b) => a + b, 0) / v.length;
  const p95 = v[Math.min(v.length - 1, Math.ceil(v.length * 0.95) - 1)];
  return {
    min: round(v[0]),
    median: round(v[Math.floor(v.length / 2)]),
    mean: round(mean),
    p95: round(p95),
    max: round(v[v.length - 1]),
  };
}
const round = (n) => Math.round(n * 10) / 10;

async function cold(runs) {
  const rows = [];
  for (let i = 0; i < runs; i++) {
    docker(["rm", "-f", NAME]);
    const hostT0 = Date.now();
    must(["run", "-d", "--rm", "--name", NAME, ...ENTRY]);
    const { readyTs } = await waitListening(NAME);
    const started = startedAt(NAME);
    rows.push({ runToReady: readyTs - hostT0, containerToReady: readyTs - started });
    docker(["rm", "-f", NAME]);
    await sleep(200);
  }
  return rows;
}

async function warm(runs) {
  docker(["rm", "-f", NAME]);
  must(["run", "-d", "--name", NAME, ...ENTRY]);
  await waitListening(NAME);
  docker(["stop", NAME]);
  let lines = logLines(NAME).length;
  const rows = [];
  for (let i = 0; i < runs; i++) {
    const hostT0 = Date.now();
    must(["start", NAME]);
    const ready = await waitListening(NAME, lines);
    lines = ready.lines;
    const started = startedAt(NAME);
    rows.push({ runToReady: ready.readyTs - hostT0, containerToReady: ready.readyTs - started });
    docker(["stop", NAME]);
    await sleep(150);
  }
  docker(["rm", "-f", NAME]);
  return rows;
}

const report = { image: IMAGE, runs: RUNS, platform: `${process.platform} ${process.arch}`, at: new Date().toISOString() };
const modes = MODE === "both" ? ["cold", "warm"] : [MODE];
for (const mode of modes) {
  const rows = mode === "cold" ? await cold(RUNS) : await warm(RUNS);
  report[mode] = {
    containerToReady: stats(rows.map((r) => r.containerToReady)),
    runToReady: stats(rows.map((r) => r.runToReady)),
    samples: rows,
  };
  const c = report[mode].containerToReady;
  const r = report[mode].runToReady;
  console.log(
    `${mode}: container→ready  min=${c.min} median=${c.median} mean=${c.mean} p95=${c.p95} max=${c.max} ms`,
  );
  console.log(
    `${mode}: run→ready        min=${r.min} median=${r.median} mean=${r.mean} p95=${r.p95} max=${r.max} ms`,
  );
}
mkdirSync(join(ROOT, "build"), { recursive: true });
writeFileSync(join(ROOT, "build", "startup-bench.json"), JSON.stringify(report, null, 2));
console.log("写入 build/startup-bench.json");
