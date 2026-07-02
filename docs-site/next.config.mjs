import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // Static export served as files by the exoharness.ai Cloudflare Worker.
  output: 'export',
  // Hosted under exoharness.ai/docs — prefixes both routes and asset URLs.
  basePath: '/docs',
  // next/image can't optimize in a static export.
  images: { unoptimized: true },
};

export default withMDX(config);
