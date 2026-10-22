// mixcomp 窗口流稳定性浸泡客户端(可恢复多段累计)。
//
// 用法: node soak-client.mjs <token> [port] [minutes] [stateFile]
// 行为: attach 第一个窗口,统计 0x01 视频帧速率、异常帧、断连次数;
//       每 60s 输出一行进度,结束时输出 SUMMARY JSON。
// 可恢复模式(传 stateFile):统计跨段累计、每 60s 落检查点(断电最多丢
//       60s 样本),SIGINT 优雅收段;累计达 targetMinutes(状态文件,默认
//       1440=24h)输出 FINAL 判定:anomalies/stalls/disconnects 全零且
//       有帧即 PASS——重连(reconnects)允许,断电恢复本就是被测场景。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
import { readFileSync, writeFileSync, existsSync, renameSync } from "node:fs";

const token = process.argv[2];
const port = process.argv[3] ?? 17601;
const minutes = Number(process.argv[4] ?? 30);
const stateFile = process.argv[5] ?? null;
if (!token) {
  console.error("用法: node soak-client.mjs <token> [port] [minutes] [stateFile]");
  process.exit(2);
}
const URL_ = `wss://127.0.0.1:${port}/ws`;

const ZERO = { frames: 0, bytes: 0, anomalies: 0, disconnects: 0, reconnects: 0, stalls: 0 };
function loadState() {
  if (!stateFile || !existsSync(stateFile)) {
    return { targetMinutes: 1440, firstStarted: Date.now(), seconds: 0, stats: { ...ZERO }, segments: 0, log: [] };
  }
  return JSON.parse(readFileSync(stateFile, "utf8"));
}
const state = loadState();
const stats = { ...state.stats };
let lastFrameAt = Date.now();
let ws = null;
let windowId = null;
let ctl = null;
let stopped = false;
let segStart = Date.now();

function saveState() {
  if (!stateFile) return;
  const tmp = `${stateFile}.tmp`;
  writeFileSync(tmp, JSON.stringify(state, null, 1));
  renameSync(tmp, stateFile);
}

process.on("SIGINT", () => {
  console.log("SIGINT:优雅收段(统计已按 60s 粒度落盘)");
  stopped = true;
});

function frame(op, payload) {
  return JSON.stringify({ v: 1, id: crypto.randomUUID(), op, ts: Date.now(), ...(payload ? { payload } : {}) });
}

async function helloAndAttach() {
  return new Promise((resolve, reject) => {
    const pending = new Map();
    const call = (op, payload) =>
      new Promise((res, rej) => {
        const raw = frame(op, payload);
        const id = JSON.parse(raw).id;
        const t = setTimeout(() => { pending.delete(id); rej(new Error(`超时 ${op}`)); }, 15000);
        pending.set(id, (env) => { clearTimeout(t); env.op.endsWith(".err") ? rej(new Error(env.payload?.message)) : res(env.payload); });
        ws.send(raw);
      });
    ws.addEventListener("message", (ev) => {
      if (typeof ev.data !== "string") {
        if (ev.data instanceof ArrayBuffer && ev.data.byteLength > 0) {
          const b = new Uint8Array(ev.data);
          if (b[0] === 0x01 && b.length >= 27) {
            stats.frames++;
            stats.bytes += b.length;
            lastFrameAt = Date.now();
          } else {
            stats.anomalies++;
          }
        }
        return;
      }
      const env = JSON.parse(ev.data);
      const w = pending.get(env.replyTo);
      if (w) { pending.delete(env.replyTo); w(env); }
    });
    ws.addEventListener("close", () => resolve({ call, closed: true }));
    ws.addEventListener("open", async () => {
      try {
        const h = await call("hello", { token, client: "soak/0.1" });
        if (!h) return reject(new Error("握手失败"));
        const list = await call("display.list", {});
        windowId = list.windows?.[0]?.id ?? null;
        if (windowId == null) return reject(new Error("没有可附加的窗口"));
        await call("display.attach", { window: windowId });
        ctl = call;
        resolve({ call, closed: false });
      } catch (e) { reject(e); }
    });
  });
}

async function run() {
  const DEADLINE = Date.now() + minutes * 60_000;
  const baseSeconds = state.seconds;
  segStart = Date.now();
  while (!stopped && Date.now() < DEADLINE) {
    try {
      ws = new WebSocket(URL_);
      ws.binaryType = "arraybuffer";
      const { closed } = await helloAndAttach();
      if (closed) {
        // 真实断连才计数;到点/收段的干净退出不算。
        stats.disconnects++;
        if (!stopped && Date.now() < DEADLINE) stats.reconnects++;
      }
      // 挂着收帧直到断开或到点。损伤门控下静态窗口零帧是健康态,不能拿
      // 帧间隔判断卡顿——活性用控制通道探测(30s 一次 sys.info,10s 超时)。
      let tick = 0;
      while (!stopped && Date.now() < DEADLINE) {
        await new Promise((r) => setTimeout(r, 1000));
        tick += 1;
        if (stateFile) {
          state.seconds = baseSeconds + (Date.now() - segStart) / 1000;
          state.stats = stats;
          const delta = Math.floor(state.seconds - baseSeconds);
          if (delta <= 1 || delta % 60 === 0) {
            saveState();
            console.log(`进度 ${(state.seconds / 60).toFixed(1)}/${(state.targetMinutes / 60).toFixed(1)}h 帧 ${stats.frames} 异常 ${stats.anomalies} 断连 ${stats.disconnects}`);
          }
        }
        if (ws.readyState !== 1) break;
        if (tick % 30 === 0 && ctl) {
          try {
            await Promise.race([
              ctl("sys.info", {}),
              new Promise((_, rej) => setTimeout(() => rej(new Error("控制通道超时")), 10_000)),
            ]);
          } catch {
            stats.stalls++;
            console.log("WARN 控制通道无响应(10s)");
          }
        }
      }
      if (ws.readyState === 1) { try { ws.close(); } catch {} await new Promise((r) => setTimeout(r, 200)); }
    } catch (e) {
      stats.reconnects++;
      console.log(`WARN 重连: ${e.message}`);
      await new Promise((r) => setTimeout(r, 2000));
    }
  }
  // 段收尾:累计落盘 + SUMMARY + PROGRESS/FINAL。
  const segSeconds = (Date.now() - segStart) / 1000;
  if (stateFile) {
    state.seconds = baseSeconds + segSeconds;
    state.segments += 1;
    state.stats = stats;
    state.log.push({
      started: new Date(segStart).toISOString(),
      ended: new Date().toISOString(),
      minutes: +(segSeconds / 60).toFixed(2),
      frames: stats.frames,
    });
    saveState();
  }
  const totalMin = state.seconds / 60;
  console.log("SUMMARY " + JSON.stringify({
    ...stats, minutes: +(segSeconds / 60).toFixed(2), windowId,
    fps: +(stats.frames / Math.max(segSeconds / 60, 1e-9) / 60).toFixed(2),
    kibps: +(stats.bytes / 1024 / Math.max(segSeconds, 1e-9) / 1024).toFixed(1),
  }));
  if (stateFile) {
    console.log(`PROGRESS 累计 ${totalMin.toFixed(1)}/${state.targetMinutes} 分钟,${state.segments} 段`);
    if (totalMin >= state.targetMinutes) {
      const pass = stats.anomalies === 0 && stats.stalls === 0 && stats.disconnects === 0 && stats.frames > 0;
      console.log(`FINAL ${pass ? "PASS" : "FAIL"} ` + JSON.stringify(state.stats));
      process.exit(pass ? 0 : 1);
    }
  }
  process.exit(0);
}

run().catch((e) => {
  // 崩溃前也要保住已累计的统计(60s 粒度兜底已在循环内落盘)。
  if (stateFile) { state.stats = stats; saveState(); }
  console.error("FATAL", e);
  process.exit(1);
});
