import { defineConfig } from "vite";

export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:7777",
      "/ws": { target: "ws://127.0.0.1:7777", ws: true },
    },
  },
});
