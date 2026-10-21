import { fileURLToPath } from "node:url";

import { defineConfig } from "vite";

export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("src", import.meta.url)),
    },
  },
  plugins: [
    {
      name: "exo-html-dev-routes",
      configureServer(server) {
        server.middlewares.use((request, _response, next) => {
          const page = request.url?.match(/^\/(v2)\/?(\?.*)?$/);
          if (page) {
            request.url = `/${page[1]}.html${page[2] ?? ""}`;
          }
          next();
        });
      },
    },
  ],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL("index.html", import.meta.url)),
        v2: fileURLToPath(new URL("v2.html", import.meta.url)),
      },
    },
  },
});
