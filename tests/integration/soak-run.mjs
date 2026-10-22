// 可恢复浸泡一键驱动:绕开「本机无法 24h 开机」限制——多段累计达 24h。
//
// 用法: node tests/integration/soak-run.mjs [--target-hours 24] [--segment-minutes 120] [--port 17631]
// 行为: 确保 mixum-soak 容器运行(默认 entrypoint,mixumd+mixcomp+foot)→
//       解析令牌 → 循环跑 soak-client.mjs 检查点段(默认每段 2h)→
//       累计满 target-hours 后按客户端 FINAL 退出码收束。
// 断电/关机:状态文件 build/soak-state.json 每 60s 落盘,重跑本脚本即续跑;
//       容器保留(可 --rm 清理)。段间客户端异常连续两次无进展则停止。
process.env.MSYS_NO_PATHCONV = "1";

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url)).replace(/\\/g, "/");
const CONTAINER = "mixum-soak";

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  return i > 0 ? Number(process.argv[i + 1]) : fallback;
}
const targetHours = arg("--target-hours", 24);
const segmentMinutes = arg("--segment-minutes", 120);
const port = arg("--port", 17631);

function sh(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: "utf8" });
  if (r.error) throw r.error;
  return (r.stdout ?? "") + (r.stderr ?? "");
}

// 1. 容器:不存在或已停都重建(重建会丢旧窗口,但浸泡统计在宿主状态文件里)。
const running = sh("docker", ["ps", "--format", "{{.Names}}"])
  .split("\n")
  .map((s) => s.trim())
  .includes(CONTAINER);
if (!running) {
  spawnSync("docker", ["rm", "-f", CONTAINER], { stdio: "ignore" });
  sh("docker", ["run", "-d", "--name", CONTAINER, "-p", `${port}:7601`, "mixumos-e2e-rust:latest"]);
  console.log(`已启动容器 ${CONTAINER}(发布 ${port})`);
} else {
  console.log(`复用运行中的容器 ${CONTAINER}`);
}

// 2. 等 READY(GUI 栈就绪,窗口可附加)。
let token = null;
const deadline = Date.now() + 60_000;
while (Date.now() < deadline) {
  token = sh("docker", ["logs", CONTAINER]).match(/READY token=(\S+)/)?.[1];
  if (token) break;
  spawnSync("sleep", ["2"]);
}
if (!token) {
  console.error("超时:容器未就绪(无 READY 行)");
  process.exit(1);
}

// 3. 状态文件:不存在则以 target-hours 初始化。
const stateFile = join(ROOT, "build", "soak-state.json");
if (!existsSync(stateFile)) {
  mkdirSync(dirname(stateFile), { recursive: true });
  writeFileSync(
    stateFile,
    JSON.stringify(
      {
        targetMinutes: Math.round(targetHours * 60),
        firstStarted: Date.now(),
        seconds: 0,
        stats: { frames: 0, bytes: 0, anomalies: 0, disconnects: 0, reconnects: 0, stalls: 0 },
        segments: 0,
        log: [],
      },
      null,
      1,
    ),
  );
  console.log(`初始化状态文件:目标 ${targetHours}h,每段 ${segmentMinutes}min`);
}
const state = () => JSON.parse(readFileSync(stateFile, "utf8"));

// 4. 段循环:每段一个 soak-client 进程,检查点续跑。
console.log(`=== 浸泡开始:累计 ${state().seconds / 60} 分钟 / 目标 ${state().targetMinutes} 分钟 ===`);
let lastSegments = state().segments;
let stallRounds = 0;
for (;;) {
  const r = spawnSync(
    process.execPath,
    [join(ROOT, "tests", "integration", "soak-client.mjs"), token, String(port), String(segmentMinutes), stateFile],
    { stdio: "inherit" },
  );
  const s = state();
  if (s.seconds / 60 >= s.targetMinutes) {
    process.exit(r.status ?? 0);
  }
  if (r.status !== 0 && s.segments === lastSegments) {
    stallRounds += 1;
    if (stallRounds >= 2) {
      console.error(`连续 ${stallRounds} 段无进展(客户端异常),停止。检查容器与网络后重跑本脚本续跑。`);
      process.exit(1);
    }
  } else {
    stallRounds = 0;
  }
  lastSegments = s.segments;
  console.log(`=== 段 ${s.segments} 结束,继续(累计 ${(s.seconds / 60).toFixed(1)} 分钟)===`);
}
