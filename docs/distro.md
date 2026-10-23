> English translation of the Chinese document. Original: [distro.zh-CN.md](distro.zh-CN.md). Technical review pending.

# mixumOS distribution build

> Status: toolchain landed (§7, 2026-10-04); the dual-architecture rebuild after
> moving mixum-agentd to an in-container cargo build is being finished (§6).
> Principle: mixumOS is a distribution; the image (OCI / rootfs) is its output, not
> the thing itself.

## 1. Shape

```
PKGBUILD (mixum-agentd / mixcomp / mixum-ctl)
   │ makepkg (arm64 / x86_64)
   ▼
pacman packages (*.pkg.tar.zst)
   │ repo-add → dist/repo/{arm64,x86_64}/
   ▼
mkosi (pacman assembles from the repo)
   ├──> mixumOS rootfs (bootable / cloud-init ready)
   └──> OCI container image (for cloud/k8s, layered from the same source)
```

Upgrade experience: inside the distro, `pacman -Syu` uses the standard Arch repos +
the mixum repo; a single component upgrades with `pacman -S mixum-agentd`.
Security updates for libraries (wlroots, openssl) come from the Arch repos; mixum
packages are repackaged, not rebuilt into a new world.

## 2. PKGBUILD conventions

- **Dynamic linking first**: mixcomp links the system wlroots0.18/libjpeg-turbo/
  json-c; Rust components dynamically link glibc by default (the Arch Rust packaging
  convention), not static musl. Static universe binaries contradict a distribution's
  philosophy; this is an explicit project decision.
- `check()`: run crate unit tests / mixcomp smoke tests.
- systemd units, sysusers.d and tmpfiles.d ship in the package (no longer scattered
  across image layers).
- Package list:

| Package | Contents | depends |
|---|---|---|
| mixum-agentd | /usr/bin/mixum-agentd, mixumd.service, sysusers | no Rust runtime; wlroots0.18 (mixcomp links at runtime, only filesystem interaction); criu (optional) |
| mixcomp | /usr/bin/mixcomp, README | wlroots0.18, libjpeg-turbo, json-c, libxkbcommon |
| mixum-ctl | /usr/bin/mixum-ctl, completions | none (Rust dynamically links the usual libraries) |
| mixum-base (meta) | default config, /var/lib/mixum layout | the three above |

- Build matrix: one PKGBUILD per `dist/pkgs/<pkg>/`; `makechrootpkg` produces one in
  each arm64/x86_64 chroot; when there is no x86_64 hardware, use qemu-user-static +
  binfmt (can be enabled inside the colima VM).

## 3. pacman repository

- Layout: `dist/repo/<arch>/` independent; `repo-add mixum.db.tar.zst
  *.pkg.tar.zst`.
- Signing: repository key and package signatures (`makepkg --sign`); the GPG public
  key is baked into the image; client/image pacman.conf points at the mixum repo and
  trusts that key.
- Distribution: v1 embeds the repo directory in the image (works offline); a public
  repo is later.

## 4. mkosi assembly

- `dist/mkosi/mkosi.conf`: distribution archlinux, `Packages=` points at the local
  repo, `CleanPackageDetect=yes` keeps it minimal.
- Container output: mkosi emits a tar → `dist/os/import-oci.sh` converts it to OCI
  layout (or `mkosi --format=oci` directly); the dual-arch manifest is merged by a
  script.
- Key in-image configuration:
  - `mixum-base.service` runs before agentd (directory/key/identity init);
  - `WLR_BACKENDS` is set by mixcomp itself (headless); the image does not intervene;
  - workspace `/var/lib/mixum/work` is declared as a separate volume (PVC in k8s; the
    snapshot engine's skip list already excludes its own storage);
  - does the container need `CAP_SYS_ADMIN`? No: overlayfs/btrfs detection is a
    runtime fallback; the default directory backend works. CRIU needs privileges;
    the image provides a `mixum-privileged` variant (stage 5).
- Bootable image (bare metal/VM outside cloud): the same mkosi config with
  `--format=disk`, cloud-init takes over first-boot configuration.

## 5. Versions and releases

- Semver: major version follows the protocol major version (envelope.v).
- Package version = crate/source version; pacman epoch manages repackaging.
- Release artifacts: `make dist` emits
  `dist/out/{repo-arm64, repo-x86_64, mixumos-<ver>-<arch>.oci.tar}` in one go.
- Reproducibility: PKGBUILD pins sources (sdist + sha256), mkosi version locked;
  build-machine differences are recorded in `dist/out/BUILDINFO`.

## 6. Where mixum-agentd is built

The Go reference implementation was removed (2026-10-16). The mixum-agentd package
is built directly from the Rust implementation by a PKGBUILD (cargo) inside the
build container: build-repo.sh exports `rust/` and `tests/cross-protocol` into the
container, installs dependencies declared by the PKGBUILD (rust/clang/cmake/perl),
and compiles natively; the binary keeps the historical name `mixumd` (e2e entry
scripts call it by that name). The old Go placeholder flow (host Go cross-compile +
placeholder wrapper) and `scripts/dist-e2e.sh` were removed with it; the full dual-
architecture rebuild awaits VM verification.

## 7. Toolchain record (2026-10-04, module/dist branch)

### 7.1 Actual shape

```
dist/pkgs/          PKGBUILD ×5 (wlroots0.18 self-built package + mixcomp
                    + mixum-agentd + mixum-ctl + mixum-base)
dist/repo/          build-repo.sh (host orchestration) + container-build.sh (two-stage in container)
dist/os/            container-image.sh (pacstrap assembly) + mkoci.sh (pure shell
                    OCI packaging with blob digest self-check) + build-image.sh (host orchestration)
scripts/            lib.sh (build container guard) + dist-e2e.sh (e2e acceptance)
Makefile            make dist / dist-repo / dist-image / dist-e2e / dist-clean
```

The single source of the version is mixum-base's pkgver (currently 0.1.0); artifact
names are `mixumos-<ver>-<arch>.oci.tar`, `mixumos-<ver>-<arch>-rootfs.tar.zst`, and
the repo at `dist/out/repo-<arch>/mixum.db.tar.zst`.

### 7.2 Deviations from the design (and why)

1. **wlroots0.18 is built by the mixum repo.** The design assumed 0.18 could be
   installed from the official repos; in practice (2026-10) both the Arch official
   repos and Arch Linux ARM only carry 0.20, while mixcomp is written against the
   0.18 API and its source tree is outside the toolchain boundary. So we build from
   upstream 0.18.2 with a minimal config (headless backend + pixman renderer only; no
   DRM/libinput/X11/EGL deps) and pin the ABI in our own repo — exactly the "bump one
   shared object for a library security update" landing point: 0.18.x fixes are
   repackaged here alone. Upstream meson's versioned name (wlroots-0.18) naturally
   matches mixcomp's probe list.
2. **mkosi is replaced by equivalent scripts (dist/os/).** The build VM is Ubuntu;
   adding mkosi would also require solving host pacman dependencies and version
   pinning. Package building already has the Arch container system, and pacstrap +
   pure-shell OCI packaging in the same container is shorter and has fewer
   dependencies. The only remaining reason to add mkosi is the bootable disk image
   (stage 5); revisit then.
3. **No makechrootpkg; run makepkg directly in an Arch container (dedicated builder
   user).** devtools' chroot copy and btrfs subvolume are duplicate isolation inside
   the container system; the container is already a clean build environment, and the
   output is still a standard pacman package (complete .PKGINFO/.BUILDINFO/MTREE).
   The trust surface = base image + pacman mirrors, recorded.
4. **The arm64 path uses Arch Linux ARM.** The official Docker images (Docker Hub and
   ghcr) have no arm64 manifest, and the Arch official repos have no arm64 packages;
   ALARM is Arch's official arm64 port with the same package names/format. The rootfs
   tar is cached at the root of dist/out/ (not committed; filename
   ArchLinuxARM-aarch64-latest.tar.gz); the download source is TUNA (the official geo
   mirror's os/ directory 404s after redirect); inside the ALARM container, pacman
   mirrors can be overridden with MIXUM_ARM_MIRROR (geo redirects are unreachable
   through some proxies; TUNA works, 2026-10-17).
5. **x86_64 path: binfmt + qemu-user** (qemu-x86_64 is registered in the build VM),
   no real hardware. mixumd host cross-compilation is not emulated; makepkg's full
   chain (wlroots build, four packages, pacstrap assembly) runs under emulation, and
   after importing the OCI image with `docker import --platform linux/amd64` the
   e2eclient full chain PASSes — x86_64 and aarch64 are both complete paths, at the
   cost of build times several times native.
6. **The v1 repository is unsigned** (SigLevel = Never); GPG signing is later (out of
   the task scope); artifacts keep the standard signature slots.
7. **Placeholder-source strategy (deprecated)**: a host Go cross-compiled
   reference/mixumd was once used as a placeholder binary; removed on 2026-10-16 with
   the Go reference implementation. The current build source is §6 (in-container
   cargo; binary keeps the historical name mixumd).
8. **mixcomp's check() is strict by default** (the historical startup segfault — six
   0.18 migration traps such as virtual-pointer timing — was fixed in
   wayland-magic.md §8.1-8.6, 71 minutes stable; strict since 2026-10-17;
   `MIXCOMP_SMOKE_STRICT=0` temporarily relaxes it). Also: the source tree contains
   historically mis-committed .o files; build() starts with `make clean` as a guard.

### 7.3 Verified (2026-10-04)

- aarch64 full chain: `make dist-repo ARCH=aarch64` produces 5 packages + repo db;
  `make dist-image ARCH=aarch64` produces rootfs (787 MB)/zstd (317 MB)/OCI
  (331 MB, blob digest self-check passes).
- e2e: after importing the dist image into docker, the e2eclient full chain PASSes
  (handshake / system info Arch Linux arm64 / task / snapshot tree / rollback / fork;
  window streaming depends on mixcomp's runtime state, compositor=false because of
  the known segfault above).
- pacman -Syu single-package upgrade: a second container uses the image as template
  and mounts dist/out/repo-aarch64; mixcomp 0.1.0-1 → 0.1.0-2; after docker restart
  the e2eclient PASSes. The same-source rebuild has the same sha256 (evidence of
  deterministic builds); version metadata advances.
- x86_64 full chain: `make dist-repo ARCH=x86_64` (qemu emulation, binfmt registered)
  produces 5 packages (including Arch-side -debug packages) + repo db;
  `make dist-image` produces rootfs (761 MB)/zstd (346 MB)/OCI (359 MB); the
  e2eclient also passes the full chain against this image.
- Dual arch: `make dist-image ARCHS="aarch64 x86_64"` additionally produces
  `mixumos-0.1.0-multi.oci.tar` (OCI image index, arm64+amd64 manifests).
- Idempotency: repeated make dist-repo / dist-image only refreshes artifacts;
  wlroots0.18 is skipped when already in the repo (delete its package file to force).

### 7.3.1 Docker Desktop/WSL2 path rebuild (2026-10-17)

After moving the build host from the colima VM to Windows Docker Desktop, a full
rebuild was measured:
- x86_64 (native) and aarch64 (qemu-user): five packages + both repos + rootfs/OCI
  all produced; after importing the dual-arch image, startup, READY, TLS handshake,
  sys.info (real snapshot backend + compositor online) acceptance PASS;
  `mixumos-0.1.0-multi.oci.tar` has both manifests.
- The in-container cargo build of mixum-agentd (after PKGBUILD.cargo became the
  default) ran for the first time and exposed/fixed makepkg CFLAGS overriding
  aws-lc-sys jitterentropy `-O0` (unset injected flags in the PKGBUILD, see 7.4).
- Host toolchain adaptations and traps are in 7.4 (git-bash path conversion,
  unconditional pacman-key init, ALARM mirror MIXUM_ARM_MIRROR); **the cargo cache
  volume is implemented** (2026-10-17): the build container mounts /cargo-home
  (registry, shared across arches) and /cargo-target (per arch) persistent volumes;
  the PKGBUILD points at them via MIXUM_CARGO_HOME/MIXUM_CARGO_TARGET_DIR; the
  volumes live outside $BUILD and survive makepkg cleanup; x86_64 measured 181 s cold
  → 45 s incremental (the rest is mixcomp meson recompilation), and arm64 under qemu
  benefits likewise; cross-compile options remain an evolution item (the path stays
  native under qemu).

### 7.4 Environment notes (trap log)

- The build container needs `--privileged`: pacstrap mounts /proc and /sys into the
  target.
- pacman sandbox: 6.x disables `DisableSandbox`; 7.1 splits it into
  `DisableSandboxFilesystem` + `DisableSandboxSyscalls`; both must be off under qemu
  emulation (use a bootstrap conf; the conf baked into the image keeps defaults).
- The ALARM rootfs keyring is uninitialized; first use needs `pacman-key --init` +
  `--populate archlinuxarm`; under qemu gpg key creation occasionally fails
  transiently, so the script retries.
- ALARM's makepkg defaults to PKGEXT=.pkg.tar.xz (Arch x86_64 uses .zst); toolchain
  globs handle both suffixes; Arch-side makepkg produces -debug packages by default.
- The build container is reused (mixum-build-<arch>), keeping the pacman cache warm.
- Docker context (the original dev environment was colima-mixumos); the host only
  needs the docker CLI.
