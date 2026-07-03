import { source } from '@/lib/source';
import { createFromSource } from 'fumadocs-core/search/server';

// Statically generated search index (no server at runtime). The client
// downloads this index and searches in-browser.
export const revalidate = false;
export const { staticGET: GET } = createFromSource(source);
