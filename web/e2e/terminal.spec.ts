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
// Overridable because target/debug/remux is sometimes the *Linux* binary:
// the remux-mobile container execs it from the bind mount, so deploys copy
// a cross-build there (see docs/STATUS.md "deploy hazard").
const BIN =
  process.env.REMUX_BIN ??
  join(dirname(fileURLToPath(import.meta.url)), "../../target/debug/remux");

let daemon: ChildProcess;
let pairToken: string;

test.beforeAll(async () => {
  const dataDir = mkdtempSync(join(tmpdir(), "remux-e2e-"));
  daemon = spawn(
    BIN,
    ["serve", "--listen", `127.0.0.1:${PORT}`, "--session", "e2emain"],
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
  // Renderer-independent: the WebGL renderer leaves .xterm-rows empty.
  return await page.evaluate(
    () => (window as unknown as { __termText?: () => string }).__termText?.() ?? ""
  );
}

test("pair, observe, take control, run a command, reconnect", async ({ page }) => {
  // --- Pairing via the QR/link URL. ---
  await page.goto(`${BASE}/#pair=${pairToken}`);

  // Connects and lands in observer mode.
  const roleChip = page.locator("#control-text");
  await expect(roleChip).toHaveText("Observer", { timeout: 10_000 });
  await expect(page.locator("#session-name")).toHaveText("e2emain");
  await expect(page.locator("#setup")).toBeHidden();

  // The tmux repaint (shell prompt) reaches the terminal.
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("$");

  // Observer swipe: scrolls tmux history (copy-mode) WITHOUT taking control
  // (glancing at a session must not resize it under the desktop user).
  await page.evaluate(() => {
    const el = document.getElementById("terminal")!;
    const rect = el.getBoundingClientRect();
    const mk = (y: number) =>
      new Touch({ identifier: 9, target: el, clientX: rect.left + 100, clientY: y });
    const opts = (y: number): TouchEventInit => ({
      touches: [mk(y)],
      changedTouches: [mk(y)],
      bubbles: true,
      cancelable: true,
    });
    const y0 = rect.top + 80;
    el.dispatchEvent(new TouchEvent("touchstart", opts(y0)));
    for (let i = 1; i <= 4; i++) {
      el.dispatchEvent(new TouchEvent("touchmove", opts(y0 + i * 40)));
    }
    el.dispatchEvent(new TouchEvent("touchend", opts(y0 + 160)));
  });
  // tmux copy-mode indicator appears; still an observer.
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .toMatch(/\[\d+\/\d+\]/);
  await expect(roleChip).toHaveText("Observer");
  // Swipe back down (finger up) far enough to exit copy-mode before typing.
  await page.evaluate(() => {
    const el = document.getElementById("terminal")!;
    const rect = el.getBoundingClientRect();
    const mk = (y: number) =>
      new Touch({ identifier: 8, target: el, clientX: rect.left + 100, clientY: y });
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
  await expect(roleChip).toHaveText("Observer");

  // --- Composer takeover: submitting as an observer requests control,
  // buffers the line, and flushes it once granted. On touch devices the
  // composer is the input surface. ---
  await page.locator("#composer-input").fill("echo e2e$((1+1))marker");
  await page.locator("#composer-input").press("Enter");
  await expect(roleChip).toContainText("Controller");
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("e2e2marker");

  // --- Font size (A- / A+) is the tmux resolution: a smaller font must fit
  // MORE columns in the same width (renderer-independent check). ---
  const cols = () =>
    page.evaluate(
      () => (window as unknown as { __termCols: () => number }).__termCols()
    );
  await page.locator("#menu-btn").click();
  const beforeCols = await cols();
  await page.locator("#font-dec").click();
  await expect.poll(cols, { timeout: 3_000 }).toBeGreaterThan(beforeCols);
  await page.locator("#font-inc").click(); // restore
  await page.locator("#menu-btn").click(); // close menu

  // Touch default: direct typing is off — terminal taps never focus xterm's
  // textarea (no on-screen keyboard).
  await page.locator(".xterm").click();
  expect(
    await page.evaluate(() => document.activeElement?.className ?? "")
  ).not.toContain("xterm-helper-textarea");
  // Enable direct typing for the raw-keyboard steps below.
  await page.locator("#menu-btn").click();
  await expect(page.locator("#termkb-btn")).toHaveText("Direct typing: off");
  await page.locator("#termkb-btn").click();
  await expect(page.locator("#termkb-btn")).toHaveText("Direct typing: on");
  await page.locator("#conn-status").click(); // close the menu
  await page.locator(".xterm").click();
  await page.keyboard.type("echo direct$((2+2))typing\n");
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("direct4typing");

  // --- Windows: create a second window via the + menu; the live topology
  // (M3a) surfaces it as a tab (M3b), then switch back via the tabs. ---
  await page.locator("#tmux-btn").click();
  await expect(page.locator("#tmux-menu")).toBeVisible();
  await page.locator("#tmux-menu .btn", { hasText: "New window" }).click();
  // The fresh window's shell replaces the old screen content.
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .not.toContain("direct4typing");

  // The tab strip renders from topology with no polling: two tabs appear.
  await expect(page.locator("#window-tabs")).toBeVisible();
  await expect(page.locator("#window-tabs .wtab")).toHaveCount(2);
  // The active tab is the new window; tap window 0's tab to switch back.
  await page.locator("#window-tabs .wtab", { hasText: /^0:/ }).click();
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("direct4typing");
  // After switching, window 0's tab is the active one.
  await expect(
    page.locator("#window-tabs .wtab.active")
  ).toHaveText(/^0:/);

  // --- Key row: ^C lives in the "…" overflow row. ---
  await page.locator("#more-key").click();
  await expect(page.locator("#keyrow-more")).toBeVisible();
  await page.locator('.key[data-key="ctrl-c"]').click();
  await page.locator("#more-key").click();
  await expect(page.locator("#keyrow-more")).toBeHidden();
  await expect
    .poll(async () => terminalText(page), { timeout: 5_000 })
    .toContain("$");

  // --- Command composer: sends a full line, records history. ---
  await page.locator("#composer-input").fill("echo composed$((3+3))ok");
  await page.locator("#composer-input").press("Enter");
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("composed6ok");
  expect(await page.locator("#composer-input").inputValue()).toBe("");

  // --- The composer chevron collapses and restores the key panel. ---
  await page.locator("#keys-toggle").click();
  await expect(page.locator("#keypanel")).toBeHidden();
  await page.locator("#keys-toggle").click();
  await expect(page.locator("#keypanel")).toBeVisible();

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

  // --- Menu: font size persists; menu closes on outside tap. ---
  await page.locator("#menu-btn").click();
  await expect(page.locator("#menu")).toBeVisible();
  await page.locator("#font-inc").click();
  // Default is 10; the earlier A-/A+ test restored it, so +1 here → 11.
  expect(await page.evaluate(() => localStorage.getItem("remux.font"))).toBe("11");
  await expect(page.locator("#notify-btn")).toHaveText("Notifications: off");
  await expect(page.locator("#termkb-btn")).toHaveText("Direct typing: on");
  await page.locator("#conn-status").click();
  await expect(page.locator("#menu")).toBeHidden();

  // --- Reload: device token persists, auto-reconnects, session survives. ---
  await page.reload();
  await expect(roleChip).toHaveText("Observer", { timeout: 10_000 });
  await expect(page.locator("#setup")).toBeHidden();
  // The tmux screen (tail of the seq output) survives the reattach.
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("200");

  // --- Session picker: list, create a new session, switch back. ---
  await page.locator("#session-name").click();
  await expect(page.locator("#session-menu")).toBeVisible();
  await expect(page.locator("#session-menu")).toContainText("e2emain");
  page.once("dialog", (d) => void d.accept("e2etwo"));
  await page
    .locator("#session-menu .btn", { hasText: "New session…" })
    .click();
  await expect(page.locator("#session-name")).toHaveText("e2etwo", {
    timeout: 10_000,
  });
  await page.locator("#session-name").click();
  await page.locator("#session-menu .btn", { hasText: "e2emain" }).click();
  await expect(page.locator("#session-name")).toHaveText("e2emain", {
    timeout: 10_000,
  });
  // The original session's screen is intact after the roundtrip.
  await expect
    .poll(async () => terminalText(page), { timeout: 10_000 })
    .toContain("200");
});

test("invalid pairing token shows setup with error", async ({ page }) => {
  await page.goto(`${BASE}/#pair=${"0".repeat(64)}`);
  await expect(page.locator("#setup")).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("#setup-error")).toContainText("Pairing failed");
});
