# remux

> Your persistent tmux session, on your phone.

remux is a small self-hosted daemon that exposes your tmux session to your own
devices as a mobile-friendly web terminal (PWA). Start work on your computer
(mosh/ssh → tmux), glance at a running job or Claude Code from your phone, type
a reply, put the phone down, and keep typing at your desk — the session never
restarts, and the terminal resizes to whichever device is active.

See [DESIGN.md](DESIGN.md) for the architecture and design review.

## How it works

```
Mac    → mosh → tmux client            ┐
                                       ├─ same tmux session
Phone  → PWA (xterm.js) → remux → tmux client (PTY)
```

- Each browser connection becomes a real tmux client attached to your session.
- New connections get an instant full repaint from tmux (no history replay).
- Observers watch; **Take control** makes the phone drive the terminal size
  (`window-size latest`). Typing on your desktop takes it back automatically.
- Locking the phone or losing signal just detaches that tmux client — your
  desktop immediately gets its dimensions back.
- Tap the session name in the top bar to switch to another tmux session or
  create a new one. The **+** button lists the session's windows (switch with
  a tap) and creates windows, splits, or cycles panes — controller only.

## Install

Requirements on the host: `tmux` ≥ 3.2 (3.3a tested), Linux or macOS.
(Windows: run it inside WSL2 — the daemon needs tmux, which has no native
Windows build.)

**Debian/Ubuntu** — grab the `.deb` from the
[releases page](https://github.com/jorgeamado/remux/releases) (a systemd
user unit ships with it, see below):

```sh
sudo apt install ./remux_*.deb
```

**macOS (Homebrew)**:

```sh
brew tap jorgeamado/remux
brew install remux
```

**Prebuilt tarballs** for Linux (x86_64/arm64) and macOS (arm64/x86_64) are
on the releases page with SHA256SUMS. **From source**:

```sh
(cd web && npm ci && npm run build)   # PWA, embedded into the binary
cargo install --path .
```

## Run

```sh
remux serve --listen <tailscale-ip>:7777
```

On startup remux prints a single-use pairing link and QR code. Open it on your
phone (over your tailnet), and the device pairs and connects. Add the page to
your home screen for the PWA experience.

### TLS (recommended, required for PWA install on iOS)

Self-signed certificates do not work with iOS PWAs. Use Tailscale's built-in
certificate support for your machine's MagicDNS name:

```sh
tailscale cert your-host.your-tailnet.ts.net
remux serve \
  --listen <tailscale-ip>:7777 \
  --tls-cert your-host.your-tailnet.ts.net.crt \
  --tls-key  your-host.your-tailnet.ts.net.key \
  --allowed-host your-host.your-tailnet.ts.net \
  --url https://your-host.your-tailnet.ts.net:7777
```

### Options

| Flag | Default | Meaning |
|---|---|---|
| `--listen` | `127.0.0.1:7777` | Bind address. Use your Tailscale IP; never a public one. |
| `--session` | `main` | tmux session to attach clients to (created if missing). |
| `--tls-cert` / `--tls-key` | — | PEM cert/key (see `tailscale cert`). |
| `--allowed-host` | — | Extra hostnames accepted by the Host/Origin guard. |
| `--url` | derived | Public URL used in the pairing QR. |
| `--no-pair` | — | Don't print a pairing token at startup. |

To pair another device later, restart the daemon (tokens are single-use and
expire after 10 minutes) or run a second `remux serve` session.

### Run as a service (Linux)

The `.deb` installs a systemd user unit
([packaging/remux.service](packaging/remux.service)); configure it via
`~/.config/remux/env`:

```sh
mkdir -p ~/.config/remux
echo 'REMUX_ARGS=--listen 100.x.y.z:7777 --no-pair' > ~/.config/remux/env
systemctl --user enable --now remux
loginctl enable-linger $USER   # keep it running while logged out
```

Installed another way? Copy the unit to `~/.config/systemd/user/` first.

## Security model

- Binds to your tailnet/localhost only; Tailscale (WireGuard) is the network
  boundary.
- Application auth on top: QR pairing → per-device revocable token
  (stored hashed in `~/.local/share/remux/devices.json`); nothing happens on a
  connection before authentication.
- Host/Origin allowlist on every request blocks DNS rebinding and cross-site
  WebSocket hijacking from malicious websites.
- The daemon runs as your user, never root, and never logs terminal I/O.

## Development

Everything runs in the devcontainer (`.devcontainer/`):

```sh
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bash

# daemon + tests
cargo test                       # unit + integration (isolated tmux socket)

# web client
cd web && npm install && npm run build    # outputs web/dist, embedded by cargo

# browser e2e (spawns the real daemon)
cd web && npx playwright install chromium && npm run e2e
```

The web client dev loop: `cd web && npm run dev` proxies `/api` and `/ws` to a
locally running daemon on `127.0.0.1:7777`.

## Notifications

Toggle **Notifications** in the aA menu (asks for browser permission). When the
session has been busy and then goes quiet — a build finished, Claude Code is
waiting for your answer — remux notifies you, but only while the app isn't
visible on screen. Note: if the OS suspends the page (locked iPhone), delivery
resumes when the socket does; real push is on the roadmap (V2, Web Push).
Every tmux session is tracked; you're only notified for the session you're
attached to.

## Roadmap

V1.x: device management UI, launchd/systemd unit files.
V2: tmux control-mode metadata (panes as tabs/cards), snapshot/delta sync with
a custom renderer, server-paged scrollback. V3: shell integration (OSC 133),
semantic command feed, Claude Code approval cards, push notifications.
