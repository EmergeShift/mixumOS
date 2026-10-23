> English translation of the Chinese document. Original: [backends.zh-CN.md](backends.zh-CN.md). Technical review pending.

# mixumOS host backends

> **Status: partial.** Apple Container / Docker / WSL 2 work; `nvx` is still an early script.
> `mixumd` (agentd) makes no container/VM assumption about its runtime: it is a Linux
> daemon that needs **a Linux userland + a workspace directory (the work root the
> snapshot engine packs) + an optional Wayland compositor (mixcomp)**. Any host
> technology that provides those three can be a backend. This page defines the four
> officially supported shapes.

## 1. Docker / OCI (default shape)

- Artifact: the OCI image from `make dist` (`mixumos-<ver>-<arch>.oci.tar`); load it
  with `docker load`/`import`, then run `docker run -p 7601:7601`.
- Use: cloud, k8s (`mixum-operator`, MixumTask CRD), local e2e development.
- Limit: CRIU inside the container needs the privileged variant (stage 5); mount a
  separate volume for the workspace.

## 2. WSL2 (Windows host, no Docker)

Since v0.1.0 the distro rootfs can be imported directly as a WSL distro — it has
full systemd and package management, so the import is one command:

```sh
bash dist/os/wsl-import.sh            # or the four manual steps above
```

**Historical verification (2026-10-17, this machine; the full import was not rerun in
this PR)**: direct rootfs import → systemd active → mixumd as a service
(User=mixum, StateDirectory) → Windows connects directly to
`wss://127.0.0.1:7601` (WSL2 localhost forwarding) → a real snapshot is created →
change a file → restore in place → materialized files match byte for byte; the
kernel field honestly records the WSL kernel (the cross-kernel restore gate applies).
In the WSL shape pacman/Arch official repositories work and single-package upgrades
work as usual. Probe: [tests/integration/wsl-probe.mjs](../tests/integration/wsl-probe.mjs).

- The workspace lives on WSL ext4, so the snapshot engine gets real ext4 semantics.
- Window streaming: mixcomp is installed as a service (mixcomp.service, since 0.1.0-3);
  GUI apps connect as the mixum user with `WAYLAND_DISPLAY=wayland-0`
  (XDG_RUNTIME_DIR=/run/mixum). **GUI apps need fonts**
  (`pacman -S ttf-dejavu noto-fonts-cjk noto-fonts noto-fonts-emoji`); without them
  the client exits immediately (observed with foot), and missing CJK fonts render
  Chinese/Japanese as boxes. **Known limit**: foot occasionally aborts (230) in a
  headless environment without WSLg, which blocks end-to-end window-stream
  verification — registration and negotiation on the mixumd/mixcomp side are fine;
  follow up on foot-on-WSL stability or re-test with a minimal Wayland client.
- Known limit: CRIU availability depends on the WSL kernel config (memory/hybrid
  snapshots degrade to unavailable in this shape); cross-kernel restore is refused
  by the kernel check gate.
- Operations: the service runs as User=mixum; do not seed workspace files as root
  by hand. Restore cleanup requires consistent file ownership; root-owned files make
  restore fail with Permission denied — correct behavior, readable error.

## 3. microsoft/nvx (microVM, watching)

[nvx](https://github.com/microsoft/nvx) is Microsoft's cross-platform microVM sandbox
for agentic workloads (on OpenVMM, Linux KVM/MSHV + Windows WHP), complementary to
mixumOS: nvx manages hypervisor-level VM startup, mixumOS is the OS/distro inside the
guest. Integration shape: use the mixumOS rootfs (OCI) as the nvx guest image, with
nvx replacing Docker as the "start an environment" backend.

Status: early (the README only has `scripts/nvx.py` booting Alpine/Ubuntu/Azure Linux;
snapshots/shared directories/GPU are not exposed). **Tracked as an evolution item**;
integrate once its guest image and API stabilize. At that point microVM isolation +
mixumOS snapshots/window streaming is the ideal combination.

## 4. k8s (mixum-operator)

The [MixumTask CRD](https://github.com/yubing0721/mixumOS-yubing-dev-fork) schedules
OCI images onto node pools (arm64/x86_64) and mounts snapshot volumes; cloud/multi-
container routing gateway is stage 5 work.

## Choosing a shape

| Shape | Good for | Prerequisite | Isolation |
|---|---|---|---|
| Docker/OCI | cloud/server/k8s | docker | container |
| WSL2 | Windows local dev/personal agent | WSL2 | container (inside a VM) |
| nvx | strongly isolated agentic sandbox (future) | OpenVMM/WHP | microVM |
| k8s | multi-container/team/cloud orchestration | cluster + operator | container + policy |
