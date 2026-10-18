// H.264/JPEG 双 codec e2e 探针(协议 §5 + 编码协商)。
//
// 用法: node tests/integration/h264-e2e.mjs <token> [port]
// 流程: 连接 A 以 codec=h264 attach → 校验应答 codec 与 Annex-B 负载、
//       统计 10s 帧率/IDR 数/码率;连接 B 以缺省 jpeg attach 同样统计,
//       输出对比;两连接串行,互不影响 broker 路由断言的同时校验共存。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

const token = process.argv[2];
const port = process.argv[3] ?? 17601;
if (!token) {
  console.error("用法: node h264-e2e.mjs <token> [port]");
  process.exit(2);
}
const URL_ = `wss://127.0.0.1:${port}/ws`;

function frame(op, payload) {
  return JSON.stringify({
    v: 1,
    id: crypto.randomUUID(),
    op,
    ts: Date.now(),
    ...(payload ? { payload } : {}),
  });
}

async function sample(codec) {
  const ws = new WebSocket(URL_);
  ws.binaryType = "arraybuffer";
  const pending = new Map();
  ws.addEventListener("message", (ev) => {
    if (typeof ev.data === "string") {
      const env = JSON.parse(ev.data);
      const w = pending.get(env.replyTo);
      if (w) {
        pending.delete(env.replyTo);
        w(env);
      }
    }
  });
  await new Promise((res, rej) => {
    ws.addEventListener("open", res, { once: true });
    ws.addEventListener("error", () => rej(new Error("连接失败")), { once: true });
  });
  const call = (op, payload) =>
    new Promise((res, rej) => {
      const raw = frame(op, payload);
      const id = JSON.parse(raw).id;
      const t = setTimeout(() => rej(new Error(`超时 ${op}`)), 15000);
      pending.set(id, (env) => {
        clearTimeout(t);
        env.op.endsWith(".err") ? rej(new Error(env.payload?.message)) : res(env.payload);
      });
      ws.send(raw);
    });

  const hello = await call("hello", { token, client: "h264-e2e/0.1" });
  if (!hello) throw new Error("握手失败");
  const list = await call("display.list", {});
  const win = list.windows?.[0]?.id;
  if (win == null) throw new Error("没有窗口");
  const attach = await call("display.attach", { window: win, fps: 30, codec });
  if (attach.codec !== codec) throw new Error(`codec 协商不符: 请求 ${codec} 实得 ${attach.codec}`);

  const stats = { frames: 0, bytes: 0, idrs: 0, badAnnexb: 0, bytesJpeg: 0 };
  ws.addEventListener("message", (ev) => {
    if (typeof ev.data !== "string" && ev.data instanceof ArrayBuffer) {
      const b = new Uint8Array(ev.data);
      if (b[0] !== 0x01 || b.length < 27) return;
      const dv = new DataView(b.buffer);
      const size = dv.getUint32(13, true);
      const idr = (dv.getUint16(17, true) & 1) !== 0;
      const payload = b.subarray(27, 27 + size);
      if (idr) stats.idrs += 1;
      if (codec === "h264") {
        // Annex-B 起始码:00 00 00 01 或 00 00 01
        const ok =
          (payload[0] === 0 && payload[1] === 0 && payload[2] === 0 && payload[3] === 1) ||
          (payload[0] === 0 && payload[1] === 0 && payload[2] === 1);
        if (!ok) stats.badAnnexb += 1;
      }
      stats.frames += 1;
      stats.bytes += size;
      if (stats.frames === 1) stats.firstFrameBytes = size;
    }
  });
  await new Promise((r) => setTimeout(r, 10000));
  ws.close();
  return { attach: attach.codec, window: win, ...stats };
}

const h264 = await sample("h264");
const jpeg = await sample("jpeg");
const mbps = (bytes) => +((bytes * 8) / 10 / 1e6).toFixed(2);
console.log(
  JSON.stringify(
    {
      h264: { ...h264, mbps: mbps(h264.bytes), avgKiB: +(h264.bytes / h264.frames / 1024).toFixed(1) },
      jpeg: { ...jpeg, mbps: mbps(jpeg.bytes), avgKiB: +(jpeg.bytes / jpeg.frames / 1024).toFixed(1) },
      ratio: +(jpeg.bytes / h264.bytes).toFixed(1),
    },
    null,
    1,
  ),
);
