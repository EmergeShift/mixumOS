// 双节点盒盖→云端续跑全链路 e2e(真实 WSS/TLS 1.3)。
//
// 前置(见 dist-e2e 同款镜像):
//   docker network create mixum-net
//   docker run -d --name mixum-device --network mixum-net -p 17601:7601 \
//     --entrypoint /bin/sh mixumos-e2e-rust -c 'mixumd --config /dev/null'
//   docker run -d --name mixum-cloud  --network mixum-net -p 17602:7601 \
//     --entrypoint /bin/sh mixumos-e2e-rust -c 'mixumd --config /dev/null'
//
// 用法: node tests/integration/sync-e2e.mjs <deviceToken> <cloudToken> [devicePort] [cloudPort]
// 链路: snap.create → snap.push(wss://mixum-cloud) → 幂等重推 →
//       云端 snap.tree/snap.restore → head 迁移;文件物化由外层 shell 校验。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

const [deviceToken, cloudToken] = process.argv.slice(2);
if (!deviceToken || !cloudToken) {
  console.error("用法: node sync-e2e.mjs <deviceToken> <cloudToken> [devicePort] [cloudPort]");
  process.exit(2);
}
const DEVICE = `wss://127.0.0.1:${process.argv[4] ?? 17601}/ws`;
const CLOUD = `wss://127.0.0.1:${process.argv[5] ?? 17602}/ws`;
const REMOTE = "wss://mixum-cloud:7601";

function frame(op, payload) {
  return JSON.stringify({
    v: 1,
    id: crypto.randomUUID(),
    op,
    ts: Date.now(),
    ...(payload === undefined ? {} : { payload }),
  });
}

class Session {
  constructor(url, token) {
    this.url = url;
    this.token = token;
    this.pending = new Map();
  }

  async connect() {
    this.ws = new WebSocket(this.url);
    this.ws.addEventListener("message", (ev) => {
      const env = JSON.parse(String(ev.data));
      const waiter = this.pending.get(env.replyTo);
      if (waiter) {
        this.pending.delete(env.replyTo);
        waiter(env);
      }
    });
    await new Promise((resolve, reject) => {
      this.ws.addEventListener("open", resolve, { once: true });
      this.ws.addEventListener("error", () => reject(new Error(`连接失败: ${this.url}`)), { once: true });
    });
    const hello = await this.call("hello", { token: this.token, client: "sync-e2e/0.1" });
    if (hello.op !== "hello.ok") throw new Error(`握手被拒: ${JSON.stringify(hello.payload)}`);
    return hello.payload;
  }

  call(op, payload, timeoutMs = 60000) {
    return new Promise((resolve, reject) => {
      const raw = frame(op, payload);
      const id = JSON.parse(raw).id;
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`超时: ${op}`));
      }, timeoutMs);
      this.pending.set(id, (env) => {
        clearTimeout(timer);
        if (env.op.endsWith(".err")) {
          reject(new Error(`${op} 失败: ${env.payload?.code} ${env.payload?.message}`));
        } else {
          resolve(env);
        }
      });
      this.ws.send(raw);
    });
  }

  close() {
    this.ws?.close();
  }
}

function expect(cond, message) {
  if (!cond) throw new Error(`断言失败: ${message}`);
  console.log(`  ok: ${message}`);
}

const device = new Session(DEVICE, deviceToken);
const cloud = new Session(CLOUD, cloudToken);

const deviceHello = await device.connect();
console.log(`device: ${deviceHello.server} ${deviceHello.arch} hostname`);
const cloudHello = await cloud.connect();
console.log(`cloud:  ${cloudHello.server} ${cloudHello.arch}`);

// 1. 设备端快照(合盖前迁移)。
const snap = await device.call("snap.create", { kind: "disk", label: "合盖前迁移" });
const nodeId = snap.payload.id;
expect(!!nodeId, `snap.create → ${nodeId}`);

// 2. 推送到云端(设备容器经 mixum-net 直连云端 7601)。
const push = await device.call("snap.push", { node: nodeId, remote: REMOTE, token: cloudToken });
const stats = push.payload;
expect(stats.node === nodeId, "push 应答节点 id 一致");
expect(Number(stats.chunksSent) >= 1, `首推 ${stats.chunksSent} 块 / ${stats.bytesSent} 字节`);

// 3. 幂等重推:零字节传输。
const again = await device.call("snap.push", { node: nodeId, remote: REMOTE, token: cloudToken });
expect(Number(again.payload.chunksSent) === 0, "重推零块(幂等,零字节传输)");

// 4. 云端树:节点已落地,origin 标记来源设备。
const tree = await cloud.call("snap.tree", {});
const remote = tree.payload.nodes.find((n) => n.id === nodeId);
expect(!!remote, "云端树包含该节点");
expect(remote.origin && remote.origin.length > 0, `origin = ${remote.origin}`);

// 5. 云端 restore(inplace):head 迁移,等待外层 shell 校验文件物化。
const restore = await cloud.call("snap.restore", { node: nodeId, strategy: "inplace" });
expect(restore.payload.restoredTo === nodeId, "云端 restore → restoredTo 一致");
const head = await cloud.call("snap.tree", {});
expect(head.payload.head === nodeId, "云端 head 已迁移到该节点(续跑起点)");

device.close();
cloud.close();
console.log(`PASS 双节点链路: ${nodeId}`);
