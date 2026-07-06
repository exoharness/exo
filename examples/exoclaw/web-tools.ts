import { lookup } from "node:dns/promises";
import { isIP } from "node:net";

import type {
  HarnessToolRegistry,
  JsonObject,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

// Host-side web access tools: web_search and web_fetch. Handlers run in the
// harness runner process on the host, so they work even when the sandbox was
// created with networking disabled.
//
// web_search picks a provider per call: Brave Search API when a key is
// configured, otherwise the key-free DuckDuckGo HTML endpoint. The Brave key
// is read from the exo secret store (`exo secret set brave-api-key ...`),
// which takes effect on the next call without a restart, with the
// BRAVE_API_KEY env var (.env) as a startup-time fallback. Set
// EXO_WEB_SEARCH_PROVIDER=brave|duckduckgo to force a provider.

const BRAVE_SECRET_ID = "brave-api-key";

const SEARCH_CACHE_TTL_MS = 15 * 60 * 1000;
const SEARCH_CACHE_MAX_ENTRIES = 50;
const DEFAULT_SEARCH_COUNT = 5;
const MAX_SEARCH_COUNT = 10;
const FETCH_TIMEOUT_MS = 12_000;
const MAX_REDIRECTS = 5;
const MAX_BODY_BYTES = 5_000_000;
const DEFAULT_FETCH_MAX_CHARS = 20_000;
const MIN_FETCH_MAX_CHARS = 1_000;
const MAX_FETCH_MAX_CHARS = 100_000;
const BROWSER_USER_AGENT =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

export type WebSearchResult = {
  title: string;
  url: string;
  snippet: string;
};

type SearchProvider = "brave" | "duckduckgo";

const searchCache = new Map<
  string,
  { expiresAtMs: number; value: JsonObject }
>();

function cacheGet(key: string): JsonObject | null {
  const entry = searchCache.get(key);
  if (entry === undefined) {
    return null;
  }
  if (entry.expiresAtMs <= Date.now()) {
    searchCache.delete(key);
    return null;
  }
  return entry.value;
}

function cacheSet(key: string, value: JsonObject): void {
  if (searchCache.size >= SEARCH_CACHE_MAX_ENTRIES) {
    const oldest = searchCache.keys().next().value;
    if (oldest !== undefined) {
      searchCache.delete(oldest);
    }
  }
  searchCache.set(key, {
    expiresAtMs: Date.now() + SEARCH_CACHE_TTL_MS,
    value,
  });
}

export function decodeEntities(text: string): string {
  return text
    .replace(/&#x([0-9a-f]+);/gi, (_, hex: string) =>
      String.fromCodePoint(Number.parseInt(hex, 16)),
    )
    .replace(/&#(\d+);/g, (_, dec: string) =>
      String.fromCodePoint(Number.parseInt(dec, 10)),
    )
    .replace(/&nbsp;/g, " ")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, "&");
}

function stripTags(html: string): string {
  return html
    .replace(/<[^>]*>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

// --- SSRF guard -------------------------------------------------------------

function isPrivateIpv4(ip: string): boolean {
  const octets = ip.split(".").map((part) => Number.parseInt(part, 10));
  if (octets.length !== 4 || octets.some((n) => Number.isNaN(n))) {
    return true;
  }
  const [a, b] = octets;
  if (a === 0 || a === 10 || a === 127) {
    return true;
  }
  if (a === 100 && b >= 64 && b <= 127) {
    return true; // CGNAT 100.64.0.0/10
  }
  if (a === 169 && b === 254) {
    return true; // link-local, incl. cloud metadata 169.254.169.254
  }
  if (a === 172 && b >= 16 && b <= 31) {
    return true;
  }
  if (a === 192 && b === 168) {
    return true;
  }
  if (a === 198 && (b === 18 || b === 19)) {
    return true; // benchmarking 198.18.0.0/15
  }
  return a >= 224; // multicast + reserved + broadcast
}

function isPrivateIpv6(ip: string): boolean {
  const lower = ip.toLowerCase();
  const mapped = /^::ffff:(\d+\.\d+\.\d+\.\d+)$/.exec(lower);
  if (mapped !== null) {
    return isPrivateIpv4(mapped[1]);
  }
  if (lower === "::" || lower === "::1" || lower.startsWith("::")) {
    return true; // unspecified, loopback, v4-compatible space
  }
  const head = Number.parseInt(lower.split(":")[0], 16);
  if (Number.isNaN(head)) {
    return true;
  }
  if ((head & 0xfe00) === 0xfc00) {
    return true; // ULA fc00::/7
  }
  if ((head & 0xffc0) === 0xfe80) {
    return true; // link-local fe80::/10
  }
  return (head & 0xff00) === 0xff00; // multicast ff00::/8
}

export function isPrivateIp(ip: string): boolean {
  const version = isIP(ip);
  if (version === 4) {
    return isPrivateIpv4(ip);
  }
  if (version === 6) {
    return isPrivateIpv6(ip);
  }
  return true; // not an IP: fail closed
}

async function assertPublicHttpUrl(url: URL): Promise<void> {
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error(`only http(s) URLs are allowed: ${url.protocol}`);
  }
  const hostname = url.hostname.replace(/^\[|\]$/g, "");
  if (
    hostname === "" ||
    hostname === "localhost" ||
    hostname.endsWith(".localhost")
  ) {
    throw new Error(`blocked host: ${url.hostname}`);
  }
  if (isIP(hostname) !== 0) {
    if (isPrivateIp(hostname)) {
      throw new Error(`blocked private address: ${hostname}`);
    }
    return;
  }
  let addresses;
  try {
    addresses = await lookup(hostname, { all: true, verbatim: true });
  } catch {
    throw new Error(`could not resolve host: ${hostname}`);
  }
  for (const { address } of addresses) {
    if (isPrivateIp(address)) {
      throw new Error(`blocked host resolving to private address: ${hostname}`);
    }
  }
}

// --- web_search providers ----------------------------------------------------

async function resolveBraveKey(context: TurnContext): Promise<string | null> {
  try {
    // getSecret takes the secret's UUID, so resolve the name via listSecrets.
    const secrets = await context.exoharness.listSecrets();
    const match = secrets.find((secret) => secret.name === BRAVE_SECRET_ID);
    if (match !== undefined) {
      const secret = await context.exoharness.getSecret(match.id);
      if (
        secret !== null &&
        secret.type === "key" &&
        secret.value.trim() !== ""
      ) {
        return secret.value.trim();
      }
    }
  } catch {
    // Fall through to the environment fallback.
  }
  const key = process.env.BRAVE_API_KEY?.trim();
  return key !== undefined && key !== "" ? key : null;
}

async function resolveSearchProvider(
  context: TurnContext,
): Promise<{ provider: SearchProvider; braveKey: string | null } | string> {
  const braveKey = await resolveBraveKey(context);
  const forced = process.env.EXO_WEB_SEARCH_PROVIDER?.trim().toLowerCase();
  if (forced === "brave" || forced === "duckduckgo") {
    return { provider: forced, braveKey };
  }
  if (forced !== undefined && forced !== "") {
    return `unknown EXO_WEB_SEARCH_PROVIDER: ${forced} (use brave or duckduckgo)`;
  }
  return { provider: braveKey !== null ? "brave" : "duckduckgo", braveKey };
}

export function normalizeDuckDuckGoUrl(href: string): string | null {
  if (href === "") {
    return null;
  }
  let resolved: URL;
  try {
    resolved = new URL(href, "https://duckduckgo.com");
  } catch {
    return null;
  }
  if (resolved.pathname === "/l/" || resolved.pathname.startsWith("/l/")) {
    const target = resolved.searchParams.get("uddg");
    return target !== null && target.startsWith("http") ? target : null;
  }
  if (resolved.hostname.endsWith("duckduckgo.com")) {
    return null; // ads and internal links
  }
  return resolved.protocol === "http:" || resolved.protocol === "https:"
    ? resolved.toString()
    : null;
}

export function parseDuckDuckGoHtml(
  html: string,
  limit: number,
): WebSearchResult[] {
  const anchors = html.matchAll(/<a\s+([^>]*)>([\s\S]*?)<\/a>/gi);
  const titles: { url: string; title: string }[] = [];
  const snippets: string[] = [];
  for (const match of anchors) {
    const attrs = match[1];
    const cls = /class="([^"]*)"/.exec(attrs)?.[1] ?? "";
    if (cls.includes("result__a")) {
      const href = /href="([^"]*)"/.exec(attrs)?.[1] ?? "";
      const url = normalizeDuckDuckGoUrl(decodeEntities(href));
      if (url !== null) {
        titles.push({ url, title: decodeEntities(stripTags(match[2])) });
      }
    } else if (cls.includes("result__snippet")) {
      snippets.push(decodeEntities(stripTags(match[2])));
    }
  }
  const seen = new Set<string>();
  const results: WebSearchResult[] = [];
  for (const [index, { url, title }] of titles.entries()) {
    if (seen.has(url)) {
      continue;
    }
    seen.add(url);
    results.push({ title, url, snippet: snippets[index] ?? "" });
    if (results.length >= limit) {
      break;
    }
  }
  return results;
}

async function searchDuckDuckGo(
  query: string,
  count: number,
): Promise<WebSearchResult[]> {
  const url = `https://html.duckduckgo.com/html/?q=${encodeURIComponent(query)}`;
  const response = await fetch(url, {
    headers: {
      "User-Agent": BROWSER_USER_AGENT,
      Accept: "text/html",
    },
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  if (!response.ok) {
    throw new Error(
      `DuckDuckGo returned HTTP ${response.status}; it may be rate limiting. Configure a Brave key (exo secret set ${BRAVE_SECRET_ID}) for a more reliable provider.`,
    );
  }
  return parseDuckDuckGoHtml(await response.text(), count);
}

async function searchBrave(
  query: string,
  count: number,
  key: string | null,
): Promise<WebSearchResult[]> {
  if (key === null) {
    throw new Error(
      `no Brave key configured; run \`exo secret set ${BRAVE_SECRET_ID} --value ...\` or set BRAVE_API_KEY, or unset EXO_WEB_SEARCH_PROVIDER`,
    );
  }
  const url = `https://api.search.brave.com/res/v1/web/search?q=${encodeURIComponent(query)}&count=${count}`;
  const response = await fetch(url, {
    headers: {
      Accept: "application/json",
      "X-Subscription-Token": key,
    },
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  if (!response.ok) {
    throw new Error(
      `Brave Search returned HTTP ${response.status}${response.status === 401 ? " (check BRAVE_API_KEY)" : ""}`,
    );
  }
  const payload = (await response.json()) as {
    web?: {
      results?: { title?: string; url?: string; description?: string }[];
    };
  };
  const results: WebSearchResult[] = [];
  for (const item of payload.web?.results ?? []) {
    if (typeof item.url !== "string" || item.url === "") {
      continue;
    }
    results.push({
      title: decodeEntities(stripTags(item.title ?? "")),
      url: item.url,
      snippet: decodeEntities(stripTags(item.description ?? "")),
    });
    if (results.length >= count) {
      break;
    }
  }
  return results;
}

// --- web_fetch extraction ----------------------------------------------------

export function extractReadableText(html: string): {
  title: string | null;
  text: string;
} {
  const titleMatch = /<title[^>]*>([\s\S]*?)<\/title>/i.exec(html);
  const rawTitle =
    titleMatch === null ? "" : decodeEntities(stripTags(titleMatch[1]));
  const title = rawTitle === "" ? null : rawTitle;
  let text = html
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(
      /<(script|style|noscript|svg|iframe|head|template)\b[\s\S]*?<\/\1>/gi,
      " ",
    )
    .replace(/<(nav|footer|aside)\b[\s\S]*?<\/\1>/gi, " ")
    .replace(
      /<a\s[^>]*href="(https?:\/\/[^"]+)"[^>]*>([\s\S]*?)<\/a>/gi,
      (_, href: string, inner: string) => {
        const label = stripTags(inner);
        return label === "" ? " " : ` [${label}](${decodeEntities(href)}) `;
      },
    )
    .replace(
      /<h([1-6])[^>]*>/gi,
      (_, level: string) => `\n\n${"#".repeat(Number(level))} `,
    )
    .replace(/<li[^>]*>/gi, "\n- ")
    .replace(/<(br|hr)\s*\/?>/gi, "\n")
    .replace(
      /<\/(p|div|section|article|h[1-6]|li|ul|ol|tr|table|blockquote|pre)>/gi,
      "\n",
    )
    .replace(/<[^>]*>/g, " ");
  text = decodeEntities(text)
    .split("\n")
    .map((line) => line.replace(/[ \t]+/g, " ").trim())
    .join("\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
  return { title, text };
}

async function readBodyWithLimit(
  response: Response,
  maxBytes: number,
): Promise<string> {
  if (response.body === null) {
    return "";
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (total < maxBytes) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    total += value.byteLength;
    chunks.push(value);
  }
  await reader.cancel().catch(() => {});
  const decoder = new TextDecoder("utf-8", { fatal: false });
  return chunks
    .map((chunk) => decoder.decode(chunk, { stream: true }))
    .join("");
}

async function fetchWithGuard(rawUrl: string): Promise<{
  finalUrl: string;
  response: Response;
}> {
  let current = new URL(rawUrl);
  for (let hop = 0; hop <= MAX_REDIRECTS; hop += 1) {
    await assertPublicHttpUrl(current);
    const response = await fetch(current, {
      redirect: "manual",
      headers: {
        "User-Agent": BROWSER_USER_AGENT,
        Accept: "text/markdown, text/html;q=0.9, text/plain;q=0.8, */*;q=0.1",
      },
      signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
    });
    const location = response.headers.get("location");
    if (
      location !== null &&
      [301, 302, 303, 307, 308].includes(response.status)
    ) {
      await response.body?.cancel().catch(() => {});
      current = new URL(location, current);
      continue;
    }
    return { finalUrl: current.toString(), response };
  }
  throw new Error(`too many redirects (max ${MAX_REDIRECTS})`);
}

// --- tool instances ----------------------------------------------------------

function webSearchTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "web_search",
      description:
        "Search the web for current information. Uses the Brave Search API when a brave-api-key secret (or BRAVE_API_KEY env) is configured, otherwise key-free DuckDuckGo. Returns normalized results with title, url, and snippet. Results are cached for 15 minutes.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          query: {
            type: "string",
            description: "Search query.",
          },
          count: {
            type: ["number", "null"],
            description:
              "Number of results to return, 1-10. Null for the default of 5.",
          },
        },
        required: ["query", "count"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const query = typeof args.query === "string" ? args.query.trim() : "";
        if (query === "") {
          return { ok: false, error: "query is required" };
        }
        const count = Math.min(
          MAX_SEARCH_COUNT,
          Math.max(
            1,
            typeof args.count === "number" && Number.isFinite(args.count)
              ? Math.floor(args.count)
              : DEFAULT_SEARCH_COUNT,
          ),
        );
        const resolved = await resolveSearchProvider(execution.context);
        if (typeof resolved === "string") {
          return { ok: false, error: resolved };
        }
        const { provider, braveKey } = resolved;
        const cacheKey = `${provider}:${count}:${query.toLowerCase()}`;
        const cached = cacheGet(cacheKey);
        if (cached !== null) {
          return { ...cached, cached: true };
        }
        let results: WebSearchResult[];
        try {
          results =
            provider === "brave"
              ? await searchBrave(query, count, braveKey)
              : await searchDuckDuckGo(query, count);
        } catch (error) {
          const message =
            error instanceof Error ? error.message : String(error);
          return { ok: false, provider, error: message };
        }
        const value: JsonObject = {
          ok: true,
          provider,
          query,
          results: results.map((result) => ({ ...result })),
        };
        if (results.length === 0 && provider === "duckduckgo") {
          value.note = `No results parsed; DuckDuckGo may be rate limiting or its markup may have changed. Consider configuring a Brave key (exo secret set ${BRAVE_SECRET_ID}).`;
        }
        cacheSet(cacheKey, value);
        return value;
      },
    },
  };
}

function webFetchTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "web_fetch",
      description:
        "Fetch an http(s) URL from the host and return the page as readable markdown-ish text (title, headings, links, body). Follows up to 5 redirects, blocks private/internal addresses, and truncates to maxChars. No JavaScript rendering; for JSON APIs the raw body is returned.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          url: {
            type: "string",
            description: "The http(s) URL to fetch.",
          },
          maxChars: {
            type: ["number", "null"],
            description:
              "Maximum characters of extracted text to return, 1000-100000. Null for the default of 20000.",
          },
        },
        required: ["url", "maxChars"],
      },
    },
    handler: {
      async execute(args: JsonObject): Promise<ToolResult> {
        const rawUrl = typeof args.url === "string" ? args.url.trim() : "";
        if (rawUrl === "") {
          return { ok: false, error: "url is required" };
        }
        let parsed: URL;
        try {
          parsed = new URL(rawUrl);
        } catch {
          return { ok: false, error: `invalid URL: ${rawUrl}` };
        }
        const maxChars = Math.min(
          MAX_FETCH_MAX_CHARS,
          Math.max(
            MIN_FETCH_MAX_CHARS,
            typeof args.maxChars === "number" && Number.isFinite(args.maxChars)
              ? Math.floor(args.maxChars)
              : DEFAULT_FETCH_MAX_CHARS,
          ),
        );
        let finalUrl: string;
        let response: Response;
        try {
          ({ finalUrl, response } = await fetchWithGuard(parsed.toString()));
        } catch (error) {
          const message =
            error instanceof Error ? error.message : String(error);
          return { ok: false, url: rawUrl, error: message };
        }
        const contentType = (
          response.headers.get("content-type") ?? ""
        ).toLowerCase();
        const isHtml =
          contentType.includes("text/html") ||
          contentType.includes("application/xhtml");
        const isText =
          isHtml ||
          contentType.startsWith("text/") ||
          contentType.includes("json") ||
          contentType.includes("xml") ||
          contentType === "";
        if (!isText) {
          await response.body?.cancel().catch(() => {});
          return {
            ok: false,
            url: rawUrl,
            finalUrl,
            status: response.status,
            error: `unsupported content type: ${contentType}`,
          };
        }
        let body: string;
        try {
          body = await readBodyWithLimit(response, MAX_BODY_BYTES);
        } catch (error) {
          const message =
            error instanceof Error ? error.message : String(error);
          return { ok: false, url: rawUrl, finalUrl, error: message };
        }
        if (!response.ok) {
          return {
            ok: false,
            url: rawUrl,
            finalUrl,
            status: response.status,
            error: `HTTP ${response.status}`,
            text: body.slice(0, 2_000),
          };
        }
        const { title, text } = isHtml
          ? extractReadableText(body)
          : { title: null, text: body.trim() };
        const truncated = text.length > maxChars;
        return {
          ok: true,
          url: rawUrl,
          finalUrl,
          status: response.status,
          contentType,
          title,
          text: truncated ? text.slice(0, maxChars) : text,
          truncated,
        };
      },
    },
  };
}

export function createWebToolInstances(): ToolInstance[] {
  return [webSearchTool(), webFetchTool()];
}

export function registerWebTools(registry: HarnessToolRegistry): void {
  for (const tool of createWebToolInstances()) {
    registry.register(tool);
  }
}
