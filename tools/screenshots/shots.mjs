// Screenshots of the dashboard (see run.sh), light and dark:
//   desktop (1280 wide, 2x): dashboard.png, dashboard-profiles.png
//   phone (390x844, 3x, touch): dashboard-phone.png, dashboard-phone-sheet.png
//     (first session's sheet open) and live-phone.png (the live view of the
//     session spry-quokka); each with a -dark twin.
// Environment: PLAYWRIGHT_DIR (where playwright-core is
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

const suffix = (theme) => (theme === "dark" ? "-dark" : "");

const newContext = async (browser, theme, options) => {
  const context = await browser.newContext({ ...options, colorScheme: theme });
  await context.addInitScript((t) => {
    try { localStorage.setItem("dash.theme", t); } catch { /* storage unavailable */ }
  }, theme);
  return context;
};

const phone = {
  viewport: { width: 390, height: 844 },
  deviceScaleFactor: 3,
  isMobile: true,
  hasTouch: true,
};

const save = async (page, file, theme) => {
  const name = `${file}${suffix(theme)}.png`;
  await page.screenshot({ path: join(process.env.OUT, name) });
  console.log(`wrote ${name}`);
};

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM, args: ["--no-sandbox"] });
try {
  await warmup(browser);
  for (const theme of ["light", "dark"]) {
    for (const { view, file, height } of views) {
      const context = await newContext(browser, theme, {
        viewport: { width: 1280, height },
        deviceScaleFactor: 2,
      });
      const page = await context.newPage();
      await page.goto(`${process.env.URL}/#${view}`, { waitUntil: "networkidle" });
      await page.waitForTimeout(6000);
      await save(page, file, theme);
      await context.close();
    }
  }

  for (const theme of ["light", "dark"]) {
    // The sessions list, then the sheet of its first session.
    const context = await newContext(browser, theme, phone);
    const page = await context.newPage();
    await page.goto(`${process.env.URL}/#sessions`, { waitUntil: "networkidle" });
    await page.waitForTimeout(6000);
    await save(page, "dashboard-phone", theme);
    await page.locator("#sessionGrid .scard .scard-title").first().click();
    await page.waitForTimeout(800);
    await save(page, "dashboard-phone-sheet", theme);
    await context.close();
  }

  // One live view at a time: every viewer resizes the shared remote page.
  for (const theme of ["light", "dark"]) {
    const context = await newContext(browser, theme, phone);
    const page = await context.newPage();
    await page.goto(`${process.env.URL}/#sessions`, { waitUntil: "networkidle" });
    const response = await page.request.get(`${process.env.URL}/api/mesh/overview`);
    const overview = await response.json();
    const session = (overview.sessions || []).find((s) => s.name === "spry-quokka");
    if (!session) throw new Error("no session named spry-quokka in /api/mesh/overview");
    await page.goto(`${process.env.URL}/handoff/?session=${session.id}`, { waitUntil: "networkidle" });
    await page.waitForTimeout(6000);
    await save(page, "live-phone", theme);
    await context.close();
  }
} finally {
  await browser.close();
}
