// Deterministic UI evidence, not a live-daemon integration test.
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
const { chromium } = createRequire(import.meta.url)("playwright");
const origin = "http://127.0.0.1:5184";
const server = spawn(process.execPath, ["node_modules/vite/bin/vite.js", "--host", "127.0.0.1", "--port", "5184", "--strictPort"], { stdio: "inherit" });
let browser;
try {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await fetch(origin).then((r) => r.ok).catch(() => false)) break;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, args: ["--no-sandbox"] });
  await mkdir("/tmp/ter-64-screenshots", { recursive: true });
  for (const theme of ["light", "dark"]) {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    await page.addInitScript((theme) => localStorage.setItem("terrarium.theme", theme), theme);
    await page.route("**/api/**", async (route) => {
      const path = new URL(route.request().url()).pathname;
      let body = [];
      if (path === "/api/session") body = { csrf: "fixture-only", password_set: true };
      else if (path === "/api/remotes/lab") body = { name: "lab", url: "https://lab.example.com", label: "Lab", status: "version_skew", read_only: true };
      else if (path.endsWith("/health")) body = { version: "0.29.0", uptime: 3600 };
      else if (path.endsWith("/counts")) body = {};
      await route.fulfill({ contentType: "application/json", body: JSON.stringify(body) });
    });
    await page.goto(`${origin}/remote/lab/projects`);
    await page.getByRole("heading", { name: "Projects", exact: true }).waitFor();
    await page.getByText("Remote: lab", { exact: true }).first().waitFor();
    await page.screenshot({ path: `/tmp/ter-64-screenshots/remote-${theme}.png`, fullPage: true });
    await page.close();
  }
} finally {
  await browser?.close();
  server.kill("SIGTERM");
}
