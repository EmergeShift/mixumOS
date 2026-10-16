# 主机集成

`mixum-ctl` 是协议 v1 的主机客户端。它通过 `wss://host:port/ws` 连接 mixumd，先发送 `hello`，随后把每个控制请求编码为 envelope，并按 `replyTo` 关联应答。服务端推送的 `task.event` 进入事件通道，视频二进制帧进入帧通道。

首次连接会计算服务器端实体证书的 SHA-256 指纹，保存到 `~/.mixum/known-hosts`，文件权限为 0600。之后同一地址必须提供相同指纹；`--fingerprint` 可在首次配对时显式指定指纹。令牌文件同样按 0600 配置，客户端不把令牌写入日志。

指纹按地址固定，解析顺序为：显式 `--fingerprint`、known-hosts 中该地址的记录、配置文件里已配对设备（`remote`）的指纹。多节点使用时，`connect` 到新节点不会影响其它地址的校验；证书轮换后对目标地址重新执行带 `--fingerprint` 的 `connect` 即完成重新配对。

合盖迁移链路是：电力事件或 `mixum-ctl power --simulate-sleep` 调用 `snap.create`（`hybrid`、`auto=true`），再调用 `snap.push` 将节点和缺失块送到远端。块传输的去重与落树由对端按协议 §6 完成，第二次推送不应产生块流量。

手动跨节点推送时，源节点令牌使用全局 `--token`，目标节点令牌使用 `snap push ... --remote-token <token>`，避免两个端点的令牌混淆。

`connect --token-path <file>` 只保存令牌文件路径；`connect --token <value>` 仅用于当前握手，不会被写入配置。

模拟合盖同样支持 `power --simulate-sleep --remote <addr> --remote-token <token>`；省略目标令牌时才回退使用源连接令牌。

macOS 的正式集成点是 IOKit 的 `IOPMSleepInterestCallback`（通过 `IORegisterForSystemPower` 注册），该回调不要求 root。`power --watch` 注册 CoreFoundation run loop，并在 `kIOMessageCanSystemSleep`/`kIOMessageSystemWillSleep` 事件中投递迁移任务；回调只做短事务。Linux 使用 systemd `sleep.target` 的 prepare/stop 单元；v1 只记录安装方式，不在客户端进程内监听 systemd。

本地视图默认监听 `127.0.0.1:7602`。根页面和窗口列表是单文件 HTML；浏览器通过 `/api/stream/<window>` WebSocket 接收 JPEG 二进制帧，按协议 27 字节帧头绘制画面并叠加光标元数据。`display record` 把指定窗口的 JPEG 逐帧保存为 `frame-0000.jpg` 等序列。

安全边界：只连接 TLS 1.3；自签证书不依赖系统 CA，而是指纹固定；TOFU 文件与配置文件不应放到共享目录；视图服务只绑定回环地址，生产环境不要将 7602 暴露到局域网。
