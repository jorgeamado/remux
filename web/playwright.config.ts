import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  timeout: 60_000,
  fullyParallel: false,
  workers: 1,
  use: {
    headless: true,
    viewport: { width: 390, height: 844 }, // iPhone-ish portrait
    hasTouch: true,
  },
});
