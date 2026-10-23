# criu-mixum

Patched CRIU 4.1 for mixumOS. Stock Arch CRIU 4.2.1 crashes in the parasite on
the mixum 6.18 kernel. The recipe removes the duplicate rseq enum block, builds
with `WERROR=0`, and installs `/usr/bin/criu`. No CRIU plugins are installed.

`mixum-base` depends on this package, so a normal mixumOS image gets the
patched binary. `mixum env create --memory-snapshots` still controls the
capability opt-in and `/proc/sys` remount.
