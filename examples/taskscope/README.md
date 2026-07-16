# taskscope — a remux demo tool

A tiny "our own htop": a live fleet monitor with no dependencies (pure bash).
It exists to exercise remux end-to-end — run it in a remux-served pane, watch it
on the phone, then let a **remux plugin render it as a custom PWA interface**.

## Run it (in the remux container)

The repo is bind-mounted into the container, so:

```sh
/workspaces/remux/examples/taskscope/taskscope
```

You get a live dashboard of workers (status, CPU%, memory, progress bars),
refreshing every second. Open the PWA and you'll see it on your phone. Ctrl-C to
quit.

## Two outputs — the point of the demo

| Mode | Output | For |
|---|---|---|
| default | a colored terminal TUI | viewing raw in the PWA today |
| `--json` | newline-delimited JSON, one object per tick | a **plugin** to render a custom interface |

```sh
taskscope --json --once      # a single structured snapshot
```
```json
{"t":0,"workers":[{"name":"indexer","status":"running","cpu":44,"mem":512,"progress":62}, …]}
```

A remux plugin consumes that clean state — not scraped terminal text — and draws
a bespoke, phone-friendly interface for it (cards, gauges, a tap-to-drill list…).
That's the "first plugin integration" this tool is here to prove.

Options: `--json` (structured stream), `--once` (one frame), `TASKSCOPE_INTERVAL`
(seconds between ticks, default 1).
