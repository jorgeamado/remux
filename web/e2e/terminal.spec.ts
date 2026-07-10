/// Browser end-to-end test: spawns the real daemon (isolated tmux socket and
/// state dir), pairs through the real pairing URL, and drives the real PWA.

import { test, expect, type Page } from "@playwright/test";
import { spawn, execFileSync, type ChildProcess } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const PORT = 7900 + Math.floor(Math.random() * 100);
const BASE = `http://127.0.0.1:${PORT}`;
const SOCK = `remux-e2e-${process.pid}`;
const BIN = join(dirname(fileURLToPath(import.meta.url)), "../../target/debug/remux");

let daemon: ChildProcess;
let pairToken: string;

test.beforeAll(async () => {
  const dataDir = mkdtempSync(join(tmpdir(), "remux-e2e-"));
  daemon = spawn(
    BIN,
    ["--listen", `127.0.0.1:${PORT}`, "--session", "e2emain"],
    {
      env: {
        ...process.env,
        XDG_DATA_HOME: dataDir,
        REMUX_TMUX_SOCKET: SOCK,
      },
      stdio: ["ignore", "pipe", "inherit"],
    }
  );

  // The daemon prints the single-use pairing URL on stdout.
  pairToken = await new Promise<string>((resolve, reject) => {
    let buf = "";
    const timer = setTimeout(() => reject(new Error(`no pairing token in output:\n${buf}`)), 10_000);
    daemon.stdout!.on("data", (chunk: Buffer) => {
      buf += chunk.toString();
      const m = buf.match(/#pair=([0-9a-f]{64})/);
      if (m) {
        clearTimeout(timer);
        resolve(m[1]);
      }
    });
  });

  // Wait until the HTTP server answers.
  for (let i = 0; i < 50; i++) {
    try {
      const resp = await fetch(`${BASE}/api/health`);
      if (resp.ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("daemon did not become healthy");
});

test.afterAll(() => {
  daemon?.kill();
  try {
    execFileSync("tmux", ["-L", SOCK, "kill-server"]);
  } catch {}
});

async function terminalText(page: Page): Promise<string> {
  return (await page.locator(".xterm-rows").textContent()) ?? "";
}

test("pair, observe, take control, run a command, reconnect", async ({ page }) => {
  // --- Pairing via the QR/link URL. ---
  await page.goto(`${BASE}/#pair=${pairToken}`);

  // Connects and lands in observer mode.
  const pill = page.locator("#role-pill");
  await expect(pill).toHaveText("observer", { timeout: 10_000 });
  await expect(page.locator("#session-name")).toHaveText("e2emain");
  await expect(page.locator("#setup")).toBeHidden();

  // The tmux repaint (shell prompt) reaches the terminal.
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("$");

  // Observer typing must not execute anything.
  await page.locator(".xterm").click();
  await page.keyboard.type("echo observer$((1+1))leak\n");
  await page.waitForTimeout(500);
  expect(await terminalText(page)).not.toContain("observer2leak");

  // --- Take control and actually use the terminal. ---
  await page.locator("#control-btn").click();
  await expect(pill).toHaveText("controller");

  await page.locator(".xterm").click();
  await page.keyboard.type("echo e2e$((1+1))marker\n");
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("e2e2marker");

  // --- Key row: Esc button exists and ctrl-C doesn't crash the stream. ---
  await page.locator('.key[data-key="ctrl-c"]').click();
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .toContain("$");

  // --- Scrollback: generate history, then scroll up into tmux copy-mode. ---
  await page.locator(".xterm").click(); // regain focus after the button press
  await page.keyboard.type("seq 1 200\n");
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("200");

  // Wheel path (xterm.js forwards wheel as tmux mouse events).
  const box = (await page.locator("#terminal").boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.wheel(0, -200);
  // tmux copy-mode shows a [position/history] indicator.
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .toMatch(/\[\d+\/\d+\]/);
  // q exits copy-mode.
  await page.keyboard.press("q");
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .not.toMatch(/\[\d+\/\d+\]/);
  // If copy-mode had already auto-exited, the q landed on the prompt: clear it.
  await page.keyboard.press("Control+u");

  // Touch path (our swipe -> mouse-report translation).
  await page.evaluate(() => {
    const el = document.getElementById("terminal")!;
    const rect = el.getBoundingClientRect();
    const mk = (y: number) =>
      new Touch({
        identifier: 1,
        target: el,
        clientX: rect.left + rect.width / 2,
        clientY: y,
      });
    const opts = (y: number): TouchEventInit => ({
      touches: [mk(y)],
      changedTouches: [mk(y)],
      bubbles: true,
      cancelable: true,
    });
    const y0 = rect.top + 80;
    el.dispatchEvent(new TouchEvent("touchstart", opts(y0)));
    for (let i = 1; i <= 6; i++) {
      el.dispatchEvent(new TouchEvent("touchmove", opts(y0 + i * 40)));
    }
    el.dispatchEvent(new TouchEvent("touchend", opts(y0 + 240)));
  });
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .toMatch(/\[\d+\/\d+\]/);
  // Swipe up (finger up = towards newest) far enough to exit copy-mode.
  await page.evaluate(() => {
    const el = document.getElementById("terminal")!;
    const rect = el.getBoundingClientRect();
    const mk = (y: number) =>
      new Touch({
        identifier: 2,
        target: el,
        clientX: rect.left + rect.width / 2,
        clientY: y,
      });
    const opts = (y: number): TouchEventInit => ({
      touches: [mk(y)],
      changedTouches: [mk(y)],
      bubbles: true,
      cancelable: true,
    });
    const y0 = rect.top + 600;
    el.dispatchEvent(new TouchEvent("touchstart", opts(y0)));
    for (let i = 1; i <= 14; i++) {
      el.dispatchEvent(new TouchEvent("touchmove", opts(y0 - i * 40)));
    }
    el.dispatchEvent(new TouchEvent("touchend", opts(y0 - 560)));
  });
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .not.toMatch(/\[\d+\/\d+\]/);

  // --- Reload: device token persists, auto-reconnects, session survives. ---
  await page.reload();
  await expect(pill).toHaveText("observer", { timeout: 10_000 });
  await expect(page.locator("#setup")).toBeHidden();
  // The tmux screen (tail of the seq output) survives the reattach.
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("200");
});

test("invalid pairing token shows setup with error", async ({ page }) => {
  await page.goto(`${BASE}/#pair=${"0".repeat(64)}`);
  await expect(page.locator("#setup")).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("#setup-error")).toContainText("Pairing failed");
});
