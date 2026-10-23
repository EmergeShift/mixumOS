// privilege sudo e2e:容器内 mixum-sudo → agentd 审批推送 → 管理员批准 → root 执行。
//
// 断言:审批事件带固化路径/sha256/peerUid;批准后 `id -u` 输出 0;
//       拒绝返回 126;shell/相对路径在审批前被拒;审计含审批人与结果。
// 前置:build/mixum-agentd-x64、build/mixum-sudo-x64(musl);镜像
//       MIXUM_E2E_IMAGE(默认 mixumos-e2e-rust,需含本分支二进制)。
// 用法: node tests/integration/privilege-e2e.mjs [port]
process.env.MSYS_NO_PATHCONV = "1";
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const IMAGE = process.env.MIXUM_E2E_IMAGE ?? "mixumos-e2e-rust";
const SUDO = process.env.MIXUM_SUDO_BIN ?? join(ROOT, "build", "mixum-sudo-x64");
const NAME = "mixum-priv-e2e";
const PORT = Number(process.argv[2] ?? 17691);

if (!existsSync(SUDO)) {
  console.error(`缺少 ${SUDO}(cargo build --release --target x86_64-unknown-linux-musl -p mixum-agentd)`);
  process.exit(2);
}

function sh(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: "utf8" });
  if (r.error) throw r.error;
  return { code: r.status ?? 0, out: (r.stdout ?? "") + (r.stderr ?? "") };
}

function run(cmd, args, timeoutMs = 15000) {
  return new Promise((resolve) => {
    const child = spawn(cmd, args, { encoding: "utf8" });
    let out = "";
    let err = "";
    child.stdout.on("data", (data) => (out += data));
    child.stderr.on("data", (data) => (err += data));
    const timer = setTimeout(() => child.kill("SIGKILL"), timeoutMs);
    child.on("error", (error) => {
      clearTimeout(timer);
      resolve({ code: -1, out, err: err + error.message });
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code: code ?? -1, out, err });
    });
  });
}

function expect(cond, message) {
  if (!cond) throw new Error(`断言失败: ${message}`);
  console.log(`  ok: ${message}`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await fn();
    if (value) return value;
    await sleep(300);
  }
  throw new Error(`超时: ${label}`);
}

/** 最小 MAP 客户端:按 replyTo 关联响应,同时收集服务端推送。 */
function ws(url) {
  const socket = new WebSocket(url);
  const pending = new Map();
  const events = [];
  const waiters = [];
  socket.binaryType = "arraybuffer";
  socket.addEventListener("message", (ev) => {
    if (typeof ev.data !== "string") return;
    const env = JSON.parse(ev.data);
    if (env.replyTo && pending.has(env.replyTo)) {
      const resolve = pending.get(env.replyTo);
      pending.delete(env.replyTo);
      resolve(env);
      return;
    }
    events.push(env);
    for (const waiter of [...waiters]) {
      if (env.op === waiter.op) {
        waiters.splice(waiters.indexOf(waiter), 1);
        clearTimeout(waiter.timer);
        waiter.resolve(env);
      }
    }
  });
  const call = (op, payload) =>
    new Promise((resolve) => {
      const env = { v: 1, id: crypto.randomUUID(), op, ts: Date.now(), payload };
      const timer = setTimeout(() => {
        pending.delete(env.id);
        resolve({ op: `${op}.err`, payload: { message: "timeout" } });
      }, 10000);
      pending.set(env.id, (reply) => {
        clearTimeout(timer);
        resolve(reply);
      });
      socket.send(JSON.stringify(env));
    });
  const nextEvent = (op, timeoutMs = 10000) =>
    new Promise((resolveCb, reject) => {
      const existing = events.find((env) => env.op === op);
      if (existing) {
        events.splice(events.indexOf(existing), 1);
        resolveCb(existing);
        return;
      }
      const waiter = { op, resolve: (env) => { events.splice(events.indexOf(env), 1); resolveCb(env); } };
      waiter.timer = setTimeout(() => {
        waiters.splice(waiters.indexOf(waiter), 1);
        reject(new Error(`超时: 未收到 ${op}`));
      }, timeoutMs);
      waiters.push(waiter);
    });
  const opened = new Promise((resolveCb, reject) => {
    socket.addEventListener("open", resolveCb, { once: true });
    socket.addEventListener("error", () => reject(new Error("ws 连接失败")), { once: true });
  });
  return { socket, call, nextEvent, opened };
}

sh("docker", ["rm", "-f", NAME]);
sh("docker", ["run", "-d", "--rm", "--name", NAME, "-p", `${PORT}:7601`, IMAGE]);

try {
  const ready = await waitFor(() => {
    const out = sh("docker", ["logs", NAME]).out;
    const token = out.match(/READY token=(\S+)/)?.[1];
    return token ? { token } : null;
  }, 30000, "容器 READY");
  console.log("1. 容器就绪");

  sh("docker", ["cp", SUDO, `${NAME}:/usr/local/bin/mixum-sudo`]);
  sh("docker", ["exec", NAME, "chmod", "+x", "/usr/local/bin/mixum-sudo"]);
  const sudo = (...args) => run("docker", ["exec", NAME, "/usr/local/bin/mixum-sudo", ...args]);

  const admin = ws(`wss://127.0.0.1:${PORT}`);
  await admin.opened;
  const hello = await admin.call("hello", { token: ready.token, client: "privilege-e2e/0.1" });
  expect(hello.op === "hello.ok", `admin hello (${hello.op})`);
  console.log("2. admin 会话就绪");

  // 1) 批准:id -u 以 root 执行。
  const pendingRun = sudo("id", "-u");
  const event = (await admin.nextEvent("privilege.request")).payload;
  expect(typeof event.id === "string" && event.id.length > 0, `请求 id ${event.id}`);
  expect(event.program === "id", `原始命令 ${event.program}`);
  expect(event.resolved.startsWith("/"), `固化路径 ${event.resolved}`);
  expect(event.sha256?.length === 64, "审批事件带 sha256");
  expect(event.peerUid === 0, `peerUid=${event.peerUid}`);
  expect(event.risk === "unknown", `risk=${event.risk}`);
  const approve = await admin.call("privilege.decision", {
    id: event.id,
    approve: true,
    reason: "e2e approve",
    scope: "once",
  });
  expect(approve.op === "privilege.decision.ok", `批准回执 (${approve.op})`);
  const approved = await pendingRun;
  expect(
    approved.code === 0,
    `sudo 退出码 ${approved.code} out=${JSON.stringify(approved.out)} err=${JSON.stringify(approved.err)}`,
  );
  expect(approved.out.trim() === "0", `id -u → ${approved.out.trim()}(root)`);
  console.log("3. 批准后以 root 执行");

  // 2) 拒绝:客户端 126,不再执行。
  const pendingDenied = sudo("id", "-u");
  const deniedEvent = (await admin.nextEvent("privilege.request")).payload;
  const deny = await admin.call("privilege.decision", {
    id: deniedEvent.id,
    approve: false,
    reason: "e2e deny",
    scope: "once",
  });
  expect(deny.op === "privilege.decision.ok", `拒绝回执 (${deny.op})`);
  const denied = await pendingDenied;
  expect(denied.code === 126, `拒绝退出码 ${denied.code}`);
  expect(/拒绝/.test(denied.err + denied.out), "拒绝原因回显");
  console.log("4. 拒绝不执行");

  // 3) shell / 相对路径:不产生审批事件,直接 126。
  const shell = await sudo("sh", "-c", "id");
  expect(shell.code === 126, `shell 退出码 ${shell.code}`);
  expect(/root shell/.test(shell.err + shell.out), "shell 被拒");
  const relative = await sudo("./id");
  expect(relative.code === 126, `相对路径退出码 ${relative.code}`);
  expect(/绝对路径/.test(relative.err + relative.out), "相对路径被拒");
  await sleep(500);
  let extra = false;
  try {
    await admin.nextEvent("privilege.request", 300);
    extra = true;
  } catch {}
  expect(!extra, "被拒请求没有审批推送");
  console.log("5. shell/相对路径在审批前被拒");

  // 4) 审计:审批人、结果、固化哈希。
  const audit = sh("docker", ["exec", NAME, "cat", "/var/lib/mixum/audit/privilege.log"]).out;
  const records = audit.trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const executed = records.find((r) => r.result === "executed");
  const deniedRecord = records.find((r) => r.result === "denied");
  expect(executed?.approver === "bootstrap-admin", `审批人 ${executed?.approver}`);
  expect(executed?.detail === "exit=0", `执行结果 ${executed?.detail}`);
  expect(executed?.request?.sha256?.length === 64, "审计记录固化哈希");
  expect(deniedRecord?.result === "denied", "审计记录拒绝");
  console.log(`6. 审计 ${records.length} 条（executed/denied）`);

  // 5) 身份 fail-closed:显式指定不存在的用户时拒绝启动。
  const failClosed = spawnSync(
    "docker",
    ["run", "--rm", "--name", `${NAME}-identity`, "-e", "MIXUM_TASK_USER=does-not-exist",
     "--entrypoint", "/usr/local/bin/mixumd", IMAGE, "--config", "/dev/null"],
    { encoding: "utf8", timeout: 20000 },
  );
  expect(failClosed.status === 1, `缺失身份退出码 ${failClosed.status}`);
  expect(/任务用户不存在/.test(failClosed.stderr + failClosed.stdout), "缺失身份报错");
  console.log("7. 任务身份 fail-closed");

  console.log("privilege-e2e PASS");
} finally {
  sh("docker", ["rm", "-f", NAME, `${NAME}-identity`]);
}
