// One-off UI screenshot at iPhone viewport against a freshly spawned daemon.
import { chromium } from "playwright";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const PORT = 7955;
const SOCK = `remux-shot-${process.pid}`;
const daemon = spawn("../target/debug/remux", ["--listen", `127.0.0.1:${PORT}`, "--session", "Dev"], {
  env: { ...process.env, XDG_DATA_HOME: mkdtempSync(join(tmpdir(), "remux-shot-")), REMUX_TMUX_SOCKET: SOCK },
  stdio: ["ignore", "pipe", "inherit"],
});
const token = await new Promise((resolve) => {
  let buf = "";
  daemon.stdout.on("data", (c) => {
    buf += c;
    const m = buf.match(/#pair=([0-9a-f]{64})/);
    if (m) resolve(m[1]);
  });
});
for (let i = 0; i < 50; i++) {
  try { if ((await fetch(`http://127.0.0.1:${PORT}/api/health`)).ok) break; } catch {}
  await new Promise((r) => setTimeout(r, 100));
}

const browser = await chromium.launch();
const ctx = await browser.newContext({
  viewport: { width: 390, height: 780 },
  deviceScaleFactor: 2,
  isMobile: true,
  hasTouch: true,
});
const page = await ctx.newPage();
await page.goto(`http://127.0.0.1:${PORT}/#pair=${token}`);
await page.waitForSelector("#control-text", { timeout: 10000 });
await page.waitForTimeout(800);
await page.screenshot({ path: "ui-observer.png" });

// take control and run a command for the controller state
await page.locator("#composer-input").fill("echo hello from remux");
await page.locator("#composer-input").press("Enter");
await page.waitForFunction(
  () => document.getElementById("control-text").textContent.includes("Controller"),
  { timeout: 10000 }
);
await page.waitForTimeout(800);
await page.screenshot({ path: "ui-controller.png" });

await browser.close();
daemon.kill();
try { execFileSync("tmux", ["-L", SOCK, "kill-server"]); } catch {}
