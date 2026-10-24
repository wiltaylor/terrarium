/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// The daemon serves the API and the artifact origin. In dev, Vite proxies to it.
const daemon = process.env.TERRARIUM_DAEMON_URL ?? "http://localhost:8080";

export default defineConfig({
  plugins: [solid()],
  server: {
    port: 5173,
    proxy: {
      "^/api(/|$)": { target: daemon, changeOrigin: true, ws: true },
      // Regex key: a bare "/a" prefix would also catch /agents and /audit.
      "^/a/": { target: daemon, changeOrigin: true },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 800,
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
