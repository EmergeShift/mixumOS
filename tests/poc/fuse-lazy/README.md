# PoC：FUSE 惰性挂载

目标：验证“挂载快照时不整盘拷贝，read 才触发数据读取”的路径。

## 运行

需要 Linux + `/dev/fuse`（Docker 用 `--privileged`）：

```bash
docker build -t mixum-fuse-poc .
docker run --rm --privileged mixum-fuse-poc sh /poc/run.sh
```

或直接在 Linux 主机：

```bash
sudo apt install fuse3 libfuse3-dev build-essential
cc -O2 -o /tmp/passthrough tests/poc/fuse-lazy/passthrough.c -lfuse3
mkdir -p /mnt && /tmp/passthrough /src /mnt -f &
```

## 判定标准

- 挂载在 `ls /mnt` 前就绪，`mount-ready-us` 记录在毫秒级；
- `ls` 只触发 getattr/readdir，不读文件内容；
- `cat /mnt/hello.txt` 只产生该文件的 read 记录；
- `cat /mnt/big.bin` 才产生 big.bin 的 read 记录（按 offset 分块）；
- 卸载干净，无残留进程。

`/tmp/fuse-reads.log` 是 read 轨迹；用它判断是否“按需读取”。

## 与 mixumOS 的距离

这是 FUSE 内核路径 + 惰性 read 的 PoC；真正的 CAS 惰性挂载还需要把 `read` 换成
“按文件 chunk 索引去 `Store` 取块”，并处理目录/元数据缓存。接口已经隔离在
`fs_read`，后续替换为 `.mart`/CAS 读取即可。

## 本机状态

Apple Silicon 上的 Colima Docker 已确认 `/dev/fuse` 与 `fuse` 文件系统存在；
本次构建时 Debian/Alpine 包下载过慢，未完成容器内编译运行。脚本与判定标准可直接在
Linux 或网络正常的 Docker 上复跑。
