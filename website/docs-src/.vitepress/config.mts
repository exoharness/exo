import { defineConfig } from "vitepress";
import { eventKindsMarkdown } from "./event-kinds";
import { darkTheme, lightTheme } from "./shiki-themes";

// Docs are served under exoharness.ai/docs by the Cloudflare Worker in
// website/. `vitepress build` emits static files straight into website/dist/docs
// (outDir below), which the Worker serves as plain assets — no nested install,
// no separate deploy.
export default defineConfig({
  base: "/docs/",
  outDir: "../dist/docs",
  cleanUrls: true,
  lang: "en",
  title: "Exo docs",
  description: "Exo is an open source runtime for agents.",
  markdown: {
    theme: { light: lightTheme, dark: darkTheme },
    config(md) {
      md.core.ruler.before("normalize", "exo-event-kinds", (state) => {
        state.src = state.src.replace("<!-- EXO_EVENT_KINDS -->", eventKindsMarkdown);
      });
    },
  },
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" }],
  ],
  themeConfig: {
    logo: {
      light: "/images/exo-badge-dark.svg",
      dark: "/images/exo-badge-light.svg",
    },
    siteTitle: "Exo",
    logoLink: { link: "/", target: "_self" },
    nav: [
      { text: "GitHub", link: "https://github.com/exoharness/exo" },
      { text: "Discord", link: "https://discord.gg/8x23hdBJU6" },
    ],
    search: { provider: "local" },
    sidebar: {
      "/archive/": [
        { text: "Current docs", link: "/" },
        { text: "Archive", link: "/archive/" },
        { text: "Earlier overview", link: "/archive/overview" },
        {
          text: "Getting started",
          link: "/archive/getting-started/",
          collapsed: true,
          items: [
            {
              text: "Installation",
              link: "/archive/getting-started/installation",
            },
            {
              text: "Your first session",
              link: "/archive/getting-started/first-session",
            },
            {
              text: "Using the CLI directly",
              link: "/archive/getting-started/quick-start",
            },
            {
              text: "A sandboxed conversation",
              link: "/archive/getting-started/sandboxed-conversation",
            },
          ],
        },
        {
          text: "Concepts",
          link: "/archive/concepts/",
          collapsed: true,
          items: [
            {
              text: "Self-agent lifecycles",
              link: "/archive/concepts/lifecycles",
            },
            { text: "Self-agent tools", link: "/archive/concepts/tools" },
            { text: "Adapters", link: "/archive/concepts/adapters" },
            {
              text: "Task scheduler",
              link: "/archive/concepts/task-scheduler",
            },
            {
              text: "The canonical agent",
              link: "/archive/concepts/canonical-agent",
            },
          ],
        },
      ],
      "/": [
        { text: "Overview", link: "/" },
        { text: "Agent definition", link: "/agent-definition" },
        {
          text: "Tutorials",
          link: "/tutorials/",
          collapsed: false,
          items: [
            { text: "Support agent", link: "/tutorials/support-agent" },
            {
              text: "Custom harness",
              link: "/tutorials/write-your-own-agent",
            },
            {
              text: "Custom coding agent",
              link: "/tutorials/custom-coding-agent",
            },
            {
              text: "Game integration",
              link: "/tutorials/game-emulator-integration",
            },
          ],
        },
        {
          text: "Architecture",
          collapsed: false,
          items: [
            { text: "Harnesses", link: "/architecture/harnesses" },
            { text: "Data model", link: "/architecture/data-model" },
            { text: "Sandboxes", link: "/architecture/sandboxes" },
            { text: "Vaults", link: "/architecture/vaults" },
          ],
        },
        {
          text: "Adapters",
          link: "/adapters/",
          collapsed: false,
          items: [
            { text: "Slack", link: "/adapters/slack" },
            { text: "Custom adapter", link: "/adapters/custom" },
          ],
        },
        { text: "Development", link: "/development/" },
      ],
    },
  },
});
