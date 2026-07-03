import { docs } from 'fumadocs-mdx:collections/server';
import { loader } from 'fumadocs-core/source';

export const source = loader({
  // basePath ('/docs') is applied by Next; keep source URLs at the root so
  // links resolve to /docs/... exactly once.
  baseUrl: '/',
  source: docs.toFumadocsSource(),
});
