# Security Policy

mixumOS runs agent workloads, holds capability tokens, snapshot data and display
streams. We take reports seriously.

## Supported versions

0.1.x receives security fixes. Pre-1.0 releases are supported on a best-effort
basis.

## Reporting a vulnerability

Please **do not open a public issue**.

- Email: **security@emergeshift.com**
- Or use GitHub private vulnerability reporting: **Security** tab → **Report a vulnerability**.

Include the affected version or commit, reproduction steps and impact. We aim
to acknowledge within 3 business days and will coordinate disclosure with you.

## Scope

- `mixum-agentd`, `mixum-ctl`, `mixum-snapshot`, `mixum-protocol`
- `mixcomp`
- the default images and the `linux-mixum-micro` kernel configuration
- the Gateway and Kubernetes operator

Out of scope: issues that require a compromised host kernel, physical access, or
unsupported third-party packages.

## Known limitations

- Node-to-node `snap.push` uses TLS 1.3 without peer-certificate pinning yet;
  only push to trusted nodes or over a private network.
- Memory snapshots (CRIU) are experimental and are not enabled by default.
- The default image uses a self-signed certificate; clients pin it on first use.
