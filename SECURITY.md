# Security policy

remux is remote shell access to your machine. Treat it accordingly:
run it only on a private network (Tailscale/WireGuard/…), never on a
public interface, always with TLS in front of real use.

## Reporting a vulnerability

Please report vulnerabilities privately via GitHub's security advisories
("Report a vulnerability" on the repository's Security tab). You should
receive a response within a week. Please do not open public issues for
security reports.

## Scope notes for researchers

- The Host/Origin allowlist, device-token auth, observer input gating,
  and the push-endpoint allowlist (SSRF guard) are security boundaries —
  bypasses are in scope.
- The admin Unix socket is authenticated by filesystem permissions
  (0600); anything reachable from another local user is in scope.
- Terminal escape-sequence handling in the PWA (xterm.js hardening,
  OSC 52 disabled) is in scope.
