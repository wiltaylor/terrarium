// Mock-only evidence for TER-714. Uses an existing Playwright installation.
/* global process, fetch, setTimeout, localStorage, console */
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
const { chromium } = createRequire(process.env.PLAYWRIGHT_PACKAGE)("playwright");
const origin = "http://127.0.0.1:5714";
const output = process.env.SCREENSHOT_DIR ?? "/tmp/ter-714-screenshots";
const server = spawn(process.execPath, ["node_modules/vite/bin/vite.js", "--host", "127.0.0.1", "--port", "5714", "--strictPort"], { env: { ...process.env, VITE_MOCK: "1" }, stdio: "inherit" });
let browser;
try {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await fetch(origin).then((r) => r.ok).catch(() => false)) break;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, args: ["--no-sandbox"] });
  await mkdir(output, { recursive: true });
  for (const theme of ["light", "dark"]) {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1100 } });
    page.on("pageerror", (error) => console.error(error.message));
    await page.addInitScript((theme) => localStorage.setItem("terrarium.theme", theme), theme);
    await page.goto(`${origin}/p/terrarium/board`);
    await page.locator('[data-card="TER-14"]').waitFor();
    await page.screenshot({ path: `${output}/board-${theme}.png`, fullPage: true });
    await page.locator('[data-card="TER-14"]').click();
    const details = page.getByRole("region", { name: "Card wait", exact: true });
    await details.waitFor();
    await details.scrollIntoViewIfNeeded();
    await page.screenshot({ path: `${output}/wait-${theme}.png`, fullPage: true });
    await page.close();
  }
} finally {
  await browser?.close();
  server.kill("SIGTERM");
}
