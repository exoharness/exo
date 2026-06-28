import { fileURLToPath } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("src", import.meta.url)),
    },
  },
  plugins: [
    react(),
    tailwindcss(),
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
