import { configDefaults, defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The web/ inspector is a standalone Vite app with its own jsdom test setup;
    // it runs via `npm test --prefix web` (wired into the root `test` script), so
    // exclude it from the root Node-environment run.
    exclude: [...configDefaults.exclude, "web/**"],
  },
});
