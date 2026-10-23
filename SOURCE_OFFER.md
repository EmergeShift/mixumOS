# Source offer for binary releases

mixumOS releases include binary artifacts (the `Image`/`bzImage` kernel, rootfs
and OCI images). Some components are licensed under GPL-2.0 or LGPL, which
require corresponding source when binaries are distributed.

## Kernel

`linux-mixum-micro` and `linux-mixum` are built from the kata-containers
6.18.35 Linux tree plus the configuration fragments in `kernel/configs/` and any
patches in `kernel/patches/` (currently none). The kernel source is GPL-2.0.

Corresponding source is available:

1. from the exact upstream tag used for the build, plus this repository's
   `kernel/configs/` and `kernel/patches/`; or
2. on request from **os@emergeshift.com** (valid for three years from the
   release date). We will provide the source on a physical medium for no more
   than the cost of distribution.

## Distribution images

The rootfs/OCI images contain Arch Linux and ALARM packages under their own
licenses. Each release publishes a `packages.txt` manifest (`pacman -Q`) next to
the image. Source for those packages is available from Arch/ALARM mirrors or on
request from **os@emergeshift.com** under the same three-year offer.

## Your rights

This offer does not limit any rights granted by GPL-2.0, LGPL-2.1 or Apache-2.0.
The Apache-2.0 license in the repository root applies to the original mixumOS
code, not to third-party components.
