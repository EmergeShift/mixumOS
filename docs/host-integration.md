# 主机集成

`mixum-ctl` 是协议 v1 的主机客户端。它通过 `wss://host:port/ws` 连接 mixumd，先发送 `hello`，随后把每个控制请求编码为 envelope，并按 `replyTo` 关联应答。服务端推送的 `task.event` 进入事件通道，视频二进制帧进入帧通道。

首次连接会计算服务器端实体证书的 SHA-256 指纹，保存到 `~/.mixum/known-hosts`，文件权限为 0600。之后同一地址必须提供相同指纹；`--fingerprint` 可在首次配对时显式指定指纹。令牌文件同样按 0600 配置，客户端不把令牌写入日志。

指纹按地址固定，解析顺序为：显式 `--fingerprint`、known-hosts 中该地址的记录、配置文件里已配对设备（`remote`）的指纹。多节点使用时，`connect` 到新节点不会影响其它地址的校验；证书轮换后对目标地址重新执行带 `--fingerprint` 的 `connect` 即完成重新配对。

合盖迁移链路是：电力事件或 `mixum-ctl power --simulate-sleep` 调用 `snap.create`（`hybrid`、`auto=true`），再调用 `snap.push` 将节点和缺失块送到远端。块传输的去重与落树由对端按协议 §6 完成，第二次推送不应产生块流量。

手动跨节点推送时，源节点令牌使用全局 `--token`，目标节点令牌使用 `snap push ... --remote-token <token>`，避免两个端点的令牌混淆。

`connect --token-path <file>` 只保存令牌文件路径；`connect --token <value>` 仅用于当前握手，不会被写入配置。

模拟合盖同样支持 `power --simulate-sleep --remote <addr> --remote-token <token>`；省略目标令牌时才回退使用源连接令牌。

macOS 的正式集成点是 IOKit 的 `IORegisterForSystemPower`，该回调不要求 root。`power --watch` 的语义：`kIOMessageCanSystemSleep`（闲置倒计时）仅放行不迁移；`kIOMessageSystemWillSleep` 触发盒盖迁移（与 `--simulate-sleep` 同一链路），系统保持唤醒等待迁移完成或超时（`--deadline-secs`，默认 30s）后才归还 `IOAllowPowerChange` 确认——迁移失败或超时也照常放行，不会挂死睡眠。Linux 使用 systemd `sleep.target` 的 system-sleep 钩子：`power --install` 生成 `/usr/lib/systemd/system-sleep/mixum-ctl`（pre 阶段同步执行迁移，deadline 20s，日志 `/var/log/mixum-power.log`），`power --print` 预览、`power --remove` 卸载；钩子以 root 运行，配置目录经 `MIXUM_CONFIG_DIR` 指向用户 `~/.mixum`，安装前需先 `connect` 配置 remote 与 token_path。

文件传输给 agent 的推荐用法：

```sh
# 主机 → 节点；stdout 只打印远端路径，便于 agent 解析
path=$(mixum-ctl file put ./input.bin --name input.bin)
mixum-ctl task submit "处理 $path" --agent shell --param cmd="cat $path"

# 节点 → 主机
mixum-ctl file get "$path" --out ./result.bin
```

`file.put` 默认落到 `workRoot/transfers/<name>`，返回绝对路径；`--json`
可拿到结构化结果。Tauri 窗口文件拖入走同一条通道，上传后把远端路径作为
文本注入目标窗口。

本地视图默认监听 `127.0.0.1:7602`。管理页（`/`）每 2s 轮询窗口列表，点击后在**独立弹出窗口**打开 `/view/<id>`——每个容器窗口一个独立视图：标题栏显示窗口标题与接管状态（badge），`display.attach` 以 fps 30 订阅 png（损伤门控下静态窗口零流量，attach 强制出首帧），按协议 27 字节帧头绘制全帧与 damage 补丁，首帧后按内容尺寸 resize，断线 1s 自动重连。视图页支持**接管与输入注入**：`接管/释放` 按钮走 `display.takeover`（agent 持有时强制接管需确认），user 模式下鼠标（motion/button/滚轮）与键盘（keysym+修饰键，F5/F11/F12 放行浏览器）经 `display.input` 注入，坐标按 canvas 缩放映射到物理像素；画布内点击后可用**宿主系统输入法**输入中文等非 ASCII 文本：页面用一个定位到点击处的隐藏输入框接收组合结果，`compositionend` 后以 `display.input {type:"text"}` 提交，mixcomp 经 text-input-v3 的 `commit_string` 交给焦点窗口（组合与候选在宿主完成，容器内无需安装输入法；仅覆盖实现 text-input-v3 的 Wayland 原生应用，XWayland/X11 客户端走 XIM，暂不覆盖）；上行控制信封经流 WS 的白名单通道转发（仅 `display.input`/`display.takeover`/`display.list`，服务端持完整凭据，快照等高权限 op 不放行）。`display record` 把指定窗口的 JPEG 逐帧保存为 `frame-0000.jpg` 等序列。控制台窗口工具栏的「音频」按钮通过 `audio.attach` 订阅容器音频（s16le 48kHz 双声道，20ms 一块），用 WebAudio 排队播放；再次点击「静音」或离开画布会 `audio.detach`。容器内需要有音频服务与采集命令（服务端 `audio_command`，如 `parec`）。

安全边界：只连接 TLS 1.3；自签证书不依赖系统 CA，而是指纹固定；TOFU 文件与配置文件不应放到共享目录；视图服务只绑定回环地址，生产环境不要将 7602 暴露到局域网。
