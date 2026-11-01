// Screenshots of the dashboard (see run.sh): the sessions and the profiles view,
// light and dark, at 2x. Environment: PLAYWRIGHT_DIR (where playwright-core is
// installed), URL (a node), OUT (the output directory), CHROMIUM (the browser).
import { createRequire } from "node:module";
import { join } from "node:path";

const require = createRequire(join(process.env.PLAYWRIGHT_DIR, "package.json"));
const { chromium } = require("playwright-core");

const views = [
  { view: "sessions", file: "dashboard", height: 720 },
  { view: "profiles", file: "dashboard-profiles", height: 680 },
];

// The thumbnails of the sessions arrive a few seconds after a page is first
// shown: look at the dashboard once before taking pictures.
const warmup = async (browser) => {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 720 } })).newPage();
  await page.goto(`${process.env.URL}/#sessions`, { waitUntil: "networkidle" });
  await page.waitForTimeout(8000);
  await page.context().close();
};

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM, args: ["--no-sandbox"] });
try {
  await warmup(browser);
  for (const theme of ["light", "dark"]) {
    for (const { view, file, height } of views) {
      const context = await browser.newContext({
        viewport: { width: 1280, height },
        deviceScaleFactor: 2,
        colorScheme: theme,
      });
      await context.addInitScript((t) => {
        try { localStorage.setItem("dash.theme", t); } catch { /* storage unavailable */ }
      }, theme);
      const page = await context.newPage();
      await page.goto(`${process.env.URL}/#${view}`, { waitUntil: "networkidle" });
      await page.waitForTimeout(6000);
      const name = `${file}${theme === "dark" ? "-dark" : ""}.png`;
      await page.screenshot({ path: join(process.env.OUT, name) });
      console.log(`wrote ${name}`);
      await context.close();
    }
  }
} finally {
  await browser.close();
}
