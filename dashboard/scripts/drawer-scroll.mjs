// Browser regression and screenshot evidence. Uses an existing Playwright install.
/* global process, fetch, setTimeout, localStorage, document, window, getComputedStyle, console */
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import assert from "node:assert/strict";
const { chromium, firefox } = createRequire(process.env.PLAYWRIGHT_PACKAGE)("playwright");
const output = process.env.SCREENSHOT_DIR ?? "/tmp/ter-956-evidence";
const origin = "http://127.0.0.1:5956";
const server = spawn(process.execPath, ["node_modules/vite/bin/vite.js", "--host", "127.0.0.1", "--port", "5956", "--strictPort"], { env: { ...process.env, VITE_MOCK: "1" }, stdio: "inherit" });
const tests = [];
try {
  await mkdir(output, { recursive: true });
  for (let i = 0; i < 100; i++) {
    if (await fetch(origin).then(r => r.ok).catch(() => false)) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  for (const [name, engine] of [["chromium", chromium], ["firefox", firefox]]) {
    let browser;
    try { browser = await engine.launch({ args: name === "chromium" ? ["--no-sandbox", "--disable-features=OverlayScrollbar"] : [] }); }
    catch (error) {
      if (name !== "firefox") throw error;
      console.log(`Firefox unavailable: ${error.message}`);
      continue;
    }
    try {
      for (const theme of ["light", "dark"]) for (const width of [1440, 760]) {
        const page = await browser.newPage({ viewport: { width, height: 900 } });
        await page.addInitScript(theme => localStorage.setItem("terrarium.theme", theme), theme);
        await page.goto(`${origin}/p/terrarium/board`);
        const password = await page.evaluate(async () => (await import("/src/api/mock/ws.ts")).MOCK_PASSWORD);
        await page.locator('input[name="password"]').fill(password);
        await page.getByRole("button", { name: "Sign in", exact: true }).click();
        await page.locator('[data-card="TER-14"]').waitFor();
        // Extend the mock board and card to exercise both scroll ranges.
        await page.evaluate(() => { document.body.style.minHeight = "4000px"; window.scrollTo(0, 200); });
        await page.locator('[data-card="TER-14"]').click();
        await page.locator(".drawer").waitFor();
        await page.evaluate(() => {
          const content = document.createElement("div");
          content.style.height = "4000px";
          content.textContent = "Long card scroll regression fixture";
          document.querySelector(".drawer").append(content);
        });
        const key = `${name}-${theme}-${width}`;
        const before = await page.addStyleTag({ content: "html { scrollbar-gutter: auto !important; overflow: auto !important; } .drawer { overscroll-behavior: auto !important; }" });
        await page.screenshot({ path: `${output}/${key}-before.png` });
        await before.evaluate(el => el.remove());
        const state = await page.evaluate(() => ({ y: window.scrollY, overflow: getComputedStyle(document.documentElement).overflowY, gutter: getComputedStyle(document.documentElement).scrollbarGutter, behavior: getComputedStyle(document.querySelector(".drawer")).overscrollBehaviorY }));
        assert.equal(state.overflow, width > 760 ? "hidden" : "visible");
        assert.equal(state.behavior, "contain");
        if (width > 760) assert.equal(state.gutter, "stable");
        const drawer = page.locator(".drawer");
        if (width > 760) {
          await drawer.evaluate(el => { el.scrollTop = 0; });
          const box = await drawer.boundingBox();
          const gutter = await drawer.evaluate(el => el.offsetWidth - el.clientWidth - 1);
          assert.ok(gutter > 0, "browser must expose a draggable classic scrollbar");
          await page.mouse.move(box.x + box.width - gutter / 2, box.y + 30);
          await page.mouse.down();
          await page.mouse.move(box.x + box.width - gutter / 2, box.y + 300, { steps: 20 });
          await page.mouse.up();
          assert.ok(await drawer.evaluate(el => el.scrollTop > 0), "scrollbar thumb drag scrolls the drawer");
          assert.equal(await page.evaluate(() => window.scrollY), state.y);
        }
        await drawer.hover();
        await page.mouse.wheel(0, 600);
        await page.waitForTimeout(200);
        assert.ok(await drawer.evaluate(el => el.scrollTop > 0));
        await drawer.evaluate(el => { el.scrollTop = el.scrollHeight; });
        await page.mouse.wheel(0, 600);
        await page.waitForTimeout(200);
        assert.equal(await page.evaluate(() => window.scrollY), state.y);
        await page.screenshot({ path: `${output}/${key}-after.png` });
        await page.keyboard.press("Escape");
        await page.waitForTimeout(100);
        assert.equal(await drawer.count(), 0);
        assert.equal(await page.evaluate(() => window.scrollY), state.y);
        await page.mouse.move(width / 2, 500);
        await page.mouse.wheel(0, 300);
        await page.waitForTimeout(200);
        assert.ok(await page.evaluate(() => window.scrollY) > state.y);
        tests.push({ name: key, status: "passed" });
        await page.close();
      }
    } finally { await browser.close(); }
  }
  await writeFile(`${output}/results.json`, JSON.stringify({ results: { tool: { name: "playwright" }, tests } }));
  console.log(`${tests.length} browser cases passed`);
} finally { server.kill("SIGTERM"); }
