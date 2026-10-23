> English translation of the Chinese document. Original: [poc-criu-wayland.zh-CN.md](poc-criu-wayland.zh-CN.md). Technical review pending.

# PoC: CRIU + Wayland session checkpoint/restore

> **Status: PoC record, not a product path.** Same machine and kernel only;
> cross-device migration is not verified.
> Result: on Apple Container 1.5.0 + `linux-mixum-micro` (6.18.35), CRIU 3.17.1
> checkpointed the whole **Sway (headless) + foot** session tree, killed it and
> restored it; Sway IPC still responds and `swaymsg -t get_tree` still shows
> `app_id: foot`. This is a same-machine, same-kernel PoC, not cross-device
> migration.

## Environment

| Item | Value |
|---|---|
| Host | macOS 27.0.1 / arm64 / Apple Container 1.5.0 |
| Kernel | `6.18.35-mixum-micro` (kata 6.18 base, 48-bit VA/PA, CRIU+Landlock+diag) |
| Image | Debian bookworm-slim |
| CRIU | 3.17.1 (Debian package) |
| Compositor/client | Sway 1.7 headless + foot 1.13.1 |
| Container | `--cap-add ALL`, PID 1 = tini |

## Key steps

```sh
# 1) PID 1 is tini so zombies are reaped after kill (otherwise restore says "Can't fork: File exists")
container run -d --name criu-poc --cap-add ALL criu-poc:1

# 2) Session script (demo user): sway + foot in the same shell tree
#    Both ends must be in the dump tree, or "Can't dump half of stream unix connection"
container exec criu-poc su -s /bin/sh demo -c \
  'nohup /usr/local/bin/session.sh >/tmp/session.log 2>&1 &'

# 3) Dump the whole session tree (not a single task)
criu dump --tree <session-shell-pid> --images-dir /tmp/d \
  --shell-job --ext-unix-sk --ghost-limit 2147483648 --file-locks -v3

# 4) Kill the session and wait for tini to reap pids
kill -9 <shell> <sway> <foot>

# 5) Restore
criu restore --images-dir /tmp/d --shell-job --ext-unix-sk -v3

# 6) Verify
SWAYSOCK=/tmp/xdg/sway-ipc.1000.<sway-pid>.sock swaymsg -t get_tree
# => "app_id": "foot"
```

## Results

- `criu dump`: `Dumping finished successfully`;
- `criu restore`: `Restore finished successfully. Tasks resumed.`；
- After restore, the Sway and foot PIDs match the snapshot, Sway IPC answers, and
  the window tree contains `app_id: foot`.

## Pitfalls

1. **Dumping only the task subtree fails** with `unix: Can't dump half of stream unix
   connection`. The other end of the Wayland connection (the compositor) must be in
   the dump tree; dump the whole container tree, or pass the task and compositor
   together with `--tree`.
2. **External Unix sockets**: `--ext-unix-sk`; without `CONFIG_UNIX_DIAG/INET_DIAG`
   CRIU cannot even collect the socket.
3. **wlroots shared memory**: enlarge `--ghost-limit` (2 GiB in the example).
4. **File locks**: `--file-locks`.
5. **PID reuse**: the old processes must actually be reaped after dump; if PID 1 does
   not reap zombies, restore fails with `Can't fork: File exists`.
6. **CRIU version**: Arch's CRIU 4.2.1 SIGSEGVs during parasite injection on this
   kernel; Debian 3.17.1 works.
7. `criu check` still reports `ns_last_pid` read-only (Apple Container's `/proc/sys`
   is read-only), but this dump/restore was not affected.

## Requirements for mixumOS

- The engine's memory/hybrid snapshot should dump the **whole container process tree**
  (or both task and mixcomp), not a single task leader.
- Pass `--ext-unix-sk --file-locks --ghost-limit` to dump and `--ext-unix-sk` to restore.
- agentd needs the compositor pid before snapshotting, or should root the tree at the
  entrypoint/PID 1.
- Cross-device memory/hybrid remains disallowed (CRIU images depend on the same kernel
  and devices).
