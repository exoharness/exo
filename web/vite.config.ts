import react from "@vitejs/plugin-react";
import { defineConfig, loadEnv } from "vite";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  const proxyTarget = env.VITE_EXO_PROXY_TARGET || "http://127.0.0.1:4766";
  const chatBridgeTarget =
    env.VITE_CHAT_BRIDGE_TARGET || "http://127.0.0.1:4767";

  return {
    plugins: [react()],
    server: {
      proxy: {
        "/exo": {
          target: proxyTarget,
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/exo/, ""),
        },
        "/chat": {
          target: chatBridgeTarget,
          changeOrigin: true,
        },
      },
    },
  };
});
