import { fileURLToPath } from "node:url";
import { configDefaults, defineConfig } from "vitest/config";

// Mirror the tsconfig path aliases so tests can import modules that use them.
export default defineConfig({
  test: {
    // The web/ inspector is a standalone Vite app with its own jsdom test setup;
    // it runs via `npm test --prefix web` (wired into the root `test` script), so
    // exclude it from the root Node-environment run.
    exclude: [...configDefaults.exclude, "web/**"],
  },
  resolve: {
    alias: {
      "@exo/harness/tool": fileURLToPath(
        new URL("./typescript/harness/tool.ts", import.meta.url),
      ),
      "@exo/harness": fileURLToPath(
        new URL("./typescript/harness/index.ts", import.meta.url),
      ),
      "@exo/model-runtime/responses": fileURLToPath(
        new URL("./typescript/model-runtime/responses.ts", import.meta.url),
      ),
    },
  },
});
