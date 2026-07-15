import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  // One long journey-style test; the tab-completion round-trips added to it
  // push slower machines past the old 60s budget.
  timeout: 120_000,
  fullyParallel: false,
  workers: 1,
  use: {
    headless: true,
    viewport: { width: 390, height: 844 }, // iPhone-ish portrait
    hasTouch: true,
  },
});
