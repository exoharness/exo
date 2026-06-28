import { fileURLToPath } from "node:url";

import { defineConfig } from "vite";

export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  plugins: [
    {
      name: "exo-chat-dev-route",
      configureServer(server) {
        server.middlewares.use((request, _response, next) => {
          if (request.url?.startsWith("/chat?")) {
            request.url = request.url.replace("/chat?", "/chat.html?");
          } else if (request.url === "/chat") {
            request.url = "/chat.html";
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
        chat: fileURLToPath(new URL("chat.html", import.meta.url)),
        main: fileURLToPath(new URL("index.html", import.meta.url)),
      },
    },
  },
});
