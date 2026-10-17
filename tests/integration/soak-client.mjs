// mixcomp 窗口流稳定性浸泡客户端。
//
// 用法: node tests/integration/soak-client.mjs <token> [port] [minutes]
// 行为: attach 第一个窗口,统计 0x01 视频帧速率、异常帧、断连次数;
//       每 60s 输出一行进度,结束时输出 SUMMARY JSON。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

const token = process.argv[2];
const port = process.argv[3] ?? 17601;
const minutes = Number(process.argv[4] ?? 30);
if (!token) {
  console.error("用法: node soak-client.mjs <token> [port] [minutes]");
  process.exit(2);
}
const URL_ = `wss://127.0.0.1:${port}/ws`;
const DEADLINE = Date.now() + minutes * 60_000;

const stats = { frames: 0, bytes: 0, anomalies: 0, disconnects: 0, reconnects: 0, stalls: 0, started: Date.now() };
let lastFrameAt = Date.now();
let ws = null;
let windowId = null;
let stopped = false;

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
        resolve({ call, closed: false });
      } catch (e) { reject(e); }
    });
  });
}

async function run() {
  while (!stopped && Date.now() < DEADLINE) {
    try {
      ws = new WebSocket(URL_);
      ws.binaryType = "arraybuffer";
      const { closed } = await helloAndAttach();
      if (closed || stopped || Date.now() >= DEADLINE) { stats.disconnects++; if (!stopped && Date.now() < DEADLINE) stats.reconnects++; }
      // 挂着收帧直到断开或到点;卡顿检测(>10s 无帧)。
      while (!stopped && Date.now() < DEADLINE) {
        await new Promise((r) => setTimeout(r, 1000));
        if (ws.readyState !== 1) break;
        if (Date.now() - lastFrameAt > 10_000) { stats.stalls++; console.log(`WARN 卡顿 ${(Date.now() - lastFrameAt) / 1000}s`); lastFrameAt = Date.now(); }
      }
    } catch (e) {
      stats.reconnects++;
      console.log(`WARN 重连: ${e.message}`);
      await new Promise((r) => setTimeout(r, 2000));
    }
  }
  const mins = (Date.now() - stats.started) / 60000;
  console.log("SUMMARY " + JSON.stringify({ ...stats, minutes: +mins.toFixed(2), windowId, fps: +(stats.frames / (mins * 60)).toFixed(2), kibps: +(stats.bytes / 1024 / (mins * 60)).toFixed(1) }));
  process.exit(0);
}

run().catch((e) => { console.error("FATAL", e); process.exit(1); });
