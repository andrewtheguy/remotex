const path = require("node:path");
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: __dirname,
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 30_000,
  outputDir: path.resolve(__dirname, "../../tmp/playwright-results"),
  reporter: "line",
  use: {
    headless: true,
    viewport: { width: 1280, height: 900 },
    permissions: ["clipboard-read", "clipboard-write"],
    trace: "retain-on-failure",
  },
});
