import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import matter from "gray-matter";
import { defineConfig } from "vitepress";

const exov2Dir = fileURLToPath(new URL("../exov2/", import.meta.url));
const exov2Pages = readdirSync(exov2Dir)
  .filter((name) => name.endsWith(".md") && name !== "index.md")
  .sort((a, b) => {
    if (a === "README.md") return -1;
    if (b === "README.md") return 1;
    return a.localeCompare(b);
  })
  .map((name) => {
    const { data } = matter(readFileSync(join(exov2Dir, name), "utf8"));
    if (typeof data.title !== "string" || !data.title.trim()) {
      throw new Error(`ExoV2 page ${name} needs a frontmatter title`);
    }
    return {
      text: data.title,
      link: `/exov2/${name.slice(0, -3)}`,
    };
  });

// Docs are served under exoharness.ai/docs by the Cloudflare Worker in
// website/. `vitepress build` emits static files straight into website/dist/docs
// (outDir below), which the Worker serves as plain assets — no nested install,
// no separate deploy.
export default defineConfig({
  base: "/docs/",
  outDir: "../dist/docs",
  cleanUrls: true,
  lang: "en",
  title: "exo docs",
  description: "Documentation for exo — a minimal system for building agents.",
  appearance: "dark",
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" }],
  ],
  themeConfig: {
    logo: {
      light: "/images/exo-badge-light.svg",
      dark: "/images/exo-badge-dark.svg",
    },
    siteTitle: "exo",
    nav: [
      { text: "Home", link: "/" },
      { text: "GitHub", link: "https://github.com/exoharness/exo" },
    ],
    search: { provider: "local" },
    socialLinks: [{ icon: "github", link: "https://github.com/exoharness/exo" }],
    sidebar: [
      { text: "Overview", link: "/" },
      {
        text: "Getting Started",
        link: "/getting-started/",
        collapsed: false,
        items: [
          { text: "Installation", link: "/getting-started/installation" },
          { text: "Your First Session", link: "/getting-started/first-session" },
          { text: "Using the CLI Directly", link: "/getting-started/quick-start" },
          {
            text: "A Sandboxed Conversation",
            link: "/getting-started/sandboxed-conversation",
          },
        ],
      },
      {
        text: "Concepts",
        link: "/concepts/",
        collapsed: false,
        items: [
          {
            text: "Exoharness & Executor",
            link: "/concepts/exoharness-and-executor",
          },
          { text: "Data Model", link: "/concepts/data-model" },
          { text: "Lifecycles", link: "/concepts/lifecycles" },
          { text: "Time Travel", link: "/concepts/time-travel" },
          { text: "Sandboxes", link: "/concepts/sandboxes" },
          {
            text: "Bindings & Secrets",
            link: "/concepts/bindings-and-secrets",
          },
          { text: "Executors & Harnesses", link: "/concepts/executors" },
          { text: "Tools", link: "/concepts/tools" },
          { text: "Adapters", link: "/concepts/adapters" },
          { text: "Task Scheduler", link: "/concepts/task-scheduler" },
          { text: "The Canonical Agent", link: "/concepts/canonical-agent" },
        ],
      },
      {
        text: "Tutorials",
        link: "/tutorials/",
        collapsed: false,
        items: [
          {
            text: "Custom Agent Quickstart",
            link: "/tutorials/write-your-own-agent",
          },
          { text: "Custom Coding Agent", link: "/tutorials/custom-coding-agent" },
          {
            text: "Game & Third-Party Tool Integration",
            link: "/tutorials/game-emulator-integration",
          },
        ],
      },
      {
        text: "ExoV2",
        link: "/exov2/",
        collapsed: false,
        items: exov2Pages,
      },
      { text: "Development", link: "/development/" },
    ],
  },
});
