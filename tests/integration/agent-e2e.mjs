// mixum-ctl agent 一键接入 e2e：目录/探测/安装/模板/失败传播。
//
// 前置：build/mixum-ctl-x64（musl x86_64）；镜像 MIXUM_E2E_IMAGE（默认
//       mixumos-e2e-rust）能起 mixumd。ctl 二进制会 docker cp 进容器，
//       在容器内对 127.0.0.1:7601 操作，避免宿主端口/证书差异。
// 用法：node tests/integration/agent-e2e.mjs [port]
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const CTL = join(ROOT, "build", "mixum-ctl-x64");
const IMAGE = process.env.MIXUM_E2E_IMAGE ?? "mixumos-e2e-rust";
const NAME = "mixum-agent-e2e";
const PORT = Number(process.argv[2] ?? 17681);

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

const ctl = (args) =>
  sh("docker", ["exec", NAME, "mixumctl", "--addr", "wss://127.0.0.1:7601",
    "--token", token, "--fingerprint", fingerprint, ...args]);

sh("docker", ["rm", "-f", NAME]);
sh("docker", ["run", "-d", "--rm", "--name", NAME, "-p", `${PORT}:7601`, IMAGE]);

const { token, fingerprint } = await waitFor(() => {
  const out = sh("docker", ["logs", NAME]).out;
  const t = out.match(/READY token=(\S+)/)?.[1];
  const f = out.match(/fingerprint: (\S+)/)?.[1];
  return t && f ? { token: t, fingerprint: f } : null;
}, 30000, "容器 READY");

try {
  sh("docker", ["cp", CTL, `${NAME}:/usr/local/bin/mixumctl`]);
  sh("docker", ["exec", NAME, "chmod", "+x", "/usr/local/bin/mixumctl"]);

  const listed = ctl(["agent", "list", "--json"]);
  expect(listed.code === 0, `agent list 退出码 ${listed.code}`);
  const start = listed.out.indexOf("[");
  const items = JSON.parse(listed.out.slice(start, listed.out.lastIndexOf("]") + 1));
  expect(items.length >= 4, `目录 ${items.length} 个 agent`);
  expect(items.every((x) => typeof x.installed === "boolean"), "list 包含 installed 状态");

  const template = ctl(["agent", "template", "codex"]);
  expect(template.out.includes("{{instruction}}"), "模板含 {{instruction}} 占位符");
  expect(template.out.includes("/root/work"), "模板含工作区约定");

  const dry = ctl(["agent", "install", "claude-code", "--dry-run"]);
  expect(dry.code === 0 && dry.out.includes("npm install -g @anthropic-ai/claude-code"),
    "dry-run 打印安装命令");
  expect(dry.out.includes("$HOME/.mixum/agents/claude-code/PROMPT.md"), "dry-run 含提示词落盘");

  const ok = ctl(["agent", "install", "claude-code", "--cmd", "echo fake-install-ok"]);
  expect(ok.code === 0, `自定义安装退出码 ${ok.code}`);
  const prompt = sh("docker", ["exec", NAME, "sh", "-c",
    "cat \"$HOME/.mixum/agents/claude-code/PROMPT.md\""]).out;
  expect(prompt.includes("mixumOS") && prompt.includes("claude-code"), "提示词已写入容器");

  const bad = ctl(["agent", "install", "codex", "--cmd", "echo boom >&2; exit 7"]);
  expect(bad.code !== 0, `失败安装返回非零（${bad.code}）`);
  expect(/失败|failed|boom/.test(bad.out), "失败原因回显");

  console.log("agent-e2e PASS");
} finally {
  sh("docker", ["rm", "-f", NAME]);
}
