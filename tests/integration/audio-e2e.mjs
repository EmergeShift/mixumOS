// 音频流 e2e：audio.attach → 容器采集命令 → 二进制音频帧(0x02) → detach。
//
// 前置：镜像 MIXUM_E2E_IMAGE（默认 mixumos-e2e-rust）可起 mixumd。
// 采集命令通过 MIXUM_AUDIO_CMD 注入：每 20ms 产出一块 3840 字节 PCM。
// 用法：node tests/integration/audio-e2e.mjs [port]
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

import { spawnSync } from "node:child_process";

const IMAGE = process.env.MIXUM_E2E_IMAGE ?? "mixumos-e2e-rust";
const NAME = "mixum-audio-e2e";
const PORT = Number(process.argv[2] ?? 17696);
const CHUNK = 3840;

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

class Client {
  constructor(url) {
    this.url = url;
    this.seq = 0;
    this.pending = new Map();
    this.audio = [];
  }
  connect() {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;
      ws.binaryType = "arraybuffer";
      ws.addEventListener("open", () => resolve());
      ws.addEventListener("error", () => reject(new Error("WS 连接失败")));
      ws.addEventListener("message", (ev) => {
        if (typeof ev.data !== "string") {
          const b = new Uint8Array(ev.data);
          if (b[0] === 0x02) {
            const dv = new DataView(ev.data, 1, 12);
            this.audio.push({
              seq: dv.getUint32(0, true),
              ptsUs: Number(dv.getBigUint64(4, true)),
              pcm: new Uint8Array(ev.data, 13),
              at: Date.now(),
            });
          }
          return;
        }
        const env = JSON.parse(ev.data);
        const cb = env.replyTo && this.pending.get(env.replyTo);
        if (cb) {
          this.pending.delete(env.replyTo);
          cb(env);
        }
      });
    });
  }
  call(op, payload) {
    return new Promise((resolve, reject) => {
      const id = crypto.randomUUID();
      const env = { v: 1, id, op, seq: ++this.seq, ts: Date.now(), payload };
      const timer = setTimeout(() => reject(new Error(`超时: ${op}`)), 15000);
      this.pending.set(id, (reply) => {
        clearTimeout(timer);
        if (reply.op.endsWith(".err")) reject(new Error(JSON.stringify(reply.payload)));
        else resolve(reply.payload);
      });
      this.ws.send(JSON.stringify(env));
    });
  }
}

sh("docker", ["rm", "-f", NAME]);
sh("docker", [
  "run", "-d", "--rm", "--name", NAME, "-p", `${PORT}:7601`,
  "-e", `MIXUM_AUDIO_CMD=while :; do head -c ${CHUNK} /dev/urandom; sleep 0.02; done`,
  IMAGE,
]);
const token = await waitFor(() => {
  const m = sh("docker", ["logs", NAME]).out.match(/READY token=(\S+)/);
  return m ? m[1] : null;
}, 30000, "容器 READY");

try {
  const client = new Client(`wss://127.0.0.1:${PORT}/ws`);
  await client.connect();
  await client.call("hello", { token, client: "audio-e2e/0.1", versions: ["1"] });

  const fmt = await client.call("audio.attach", {});
  expect(fmt.sampleRate === 48000, `采样率 ${fmt.sampleRate}`);
  expect(fmt.channels === 2, `声道 ${fmt.channels}`);
  expect(fmt.format === "s16le" && fmt.chunkMs === 20, `格式 ${fmt.format}/${fmt.chunkMs}ms`);

  await waitFor(() => client.audio.length >= 5, 10000, "5 个音频帧");
  const frames = client.audio.slice(0, 5);
  expect(frames.every((f) => f.pcm.length === CHUNK), `帧负载 ${CHUNK} 字节`);
  expect(frames[1].seq === frames[0].seq + 1, "序号连续");
  expect(frames.some((f) => f.pcm.some((v) => v !== 0)), "PCM 非空");
  const steps = frames.slice(1).map((f, i) => (f.ptsUs - frames[i].ptsUs) / 1000);
  expect(steps.every((ms) => ms > 5 && ms < 200), `采集时间戳步长 ${steps.map((m) => m.toFixed(0)).join("/")}ms`);
  expect(frames[4].ptsUs > frames[0].ptsUs, "PTS 单调");
  const gap = frames[4].at - frames[0].at;
  expect(gap > 30 && gap < 2000, `5 帧耗时 ${gap}ms`);

  await client.call("audio.detach", {});
  await sleep(300);
  const afterDetach = client.audio.length;
  await sleep(600);
  expect(client.audio.length - afterDetach <= 1,
    `detach 后不再收到音频（300ms 后 ${afterDetach}，再 600ms 后 ${client.audio.length}）`);

  const again = await client.call("audio.attach", {});
  expect(again.sampleRate === 48000, "重新 attach 成功");
  await waitFor(() => client.audio.length > afterDetach + 1, 10000, "重挂后恢复音频");
  await client.call("audio.detach", {});

  console.log("audio-e2e PASS");
} finally {
  sh("docker", ["rm", "-f", NAME]);
}
