# Third-party notices

mixumOS is Apache-2.0, but it builds on and distributes third-party software.
This file summarizes the components that matter for redistribution. It is not a
substitute for the license text shipped by each upstream project.

## In this repository

| Component | License | Notes |
|---|---|---|
| wlroots 0.18.2 | MIT | built from the unmodified upstream tarball; not vendored |
| Wayland / wayland-protocols | MIT | generated protocol code |
| libxkbcommon | MIT | |
| json-c | MIT | |
| libjpeg-turbo | IJG / BSD-3-Clause | |
| zstd | BSD-3-Clause | |
| libpng | libpng / zlib | |
| xorg-xwayland | MIT | optional X11 backend |
| Inter SemiBold | SIL OFL 1.1 | wordmark outlines only; no font files are shipped |
| Rust crates | MIT / Apache-2.0 / BSD / ISC / Unicode-3.0; a few MPL-2.0 (option-ext) | see `rust/Cargo.lock`; MPL notices retained |
| Console build deps | MIT / ISC / Apache-2.0; `caniuse-lite` data is CC-BY-4.0 | see `console/package-lock.json` |

## In release images and kernel binaries

| Component | License | Compliance |
|---|---|---|
| Linux kernel / kata 6.18.35 (`linux-mixum-micro`) | GPL-2.0 | source offer in `SOURCE_OFFER.md`; the whole `kernel/` directory is GPL-2.0 (see `kernel/NOTICE`) |
| Arch Linux / ALARM packages | upstream licenses | package manifest is published with each image; source is available from Arch/ALARM or on request |
| CRIU (not bundled) | GPL-2.0 / LGPL-2.1 | optional runtime dependency |
| PulseAudio (image) | LGPL-2.1 | dynamically linked |

If you redistribute a mixumOS image, keep this file, the upstream license texts,
and the package manifest with it.
