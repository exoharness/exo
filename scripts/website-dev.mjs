import { createServer as createViteServer } from "vite";
import { createServer as createVitePressServer } from "vitepress";

const docsServer = await createVitePressServer("website/docs-src", {
  host: "127.0.0.1",
  port: 5180,
});

try {
  await docsServer.listen();
  const docsAddress = docsServer.httpServer.address();
  if (!docsAddress || typeof docsAddress === "string") {
    throw new Error("Could not determine the docs server port");
  }

  const websiteServer = await createViteServer({
    configFile: "website/vite.config.js",
    server: {
      proxy: {
        "/docs": {
          target: `http://127.0.0.1:${docsAddress.port}`,
          ws: true,
        },
      },
    },
  });

  try {
    await websiteServer.listen();
    websiteServer.printUrls();
  } catch (error) {
    await websiteServer.close();
    throw error;
  }

  process.once("SIGINT", async () => {
    await Promise.all([websiteServer.close(), docsServer.close()]);
    process.exit(0);
  });
} catch (error) {
  await docsServer.close();
  throw error;
}
