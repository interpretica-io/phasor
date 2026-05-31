# Security Policy

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue.

Use GitHub's private vulnerability reporting:
**Security → Report a vulnerability** on
<https://github.com/interpretica-io/phasor/security/advisories/new>.

We aim to acknowledge reports within a few days and will coordinate a fix and
disclosure with you.

## Supported versions

phasor is pre-1.0; only the latest release receives security fixes.

## Threat model — please read

phasor is a **local developer tool**, not a multi-user service. Two areas are
security-relevant by design:

- **The web dashboard (`phasor serve`)** bridges a WebSocket to a PTY running
  `tmux attach`, i.e. it exposes **full interactive shell access** to whoever
  can reach the port. It therefore binds to **`127.0.0.1` only**. Do not expose
  it to other hosts (e.g. via a reverse proxy, `0.0.0.0`, or port forwarding)
  on an untrusted network — doing so hands out a shell. There is no
  authentication layer.
- **Window ids** from the browser (`?w=@N`) are interpolated into a shell
  command and are strictly validated (`@` followed by digits only) before use.

If you find a way to escape that validation, reach a session you shouldn't, or
otherwise gain access beyond the local user's own agents, that's a vulnerability
— please report it.
