const SESSION_PATH_PATTERN = /^\/chat\/(?:s|agent)\/([A-Za-z0-9_-]{8,128})\/?$/;

const SECURITY_HEADERS = {
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "Content-Security-Policy":
    "default-src 'self'; connect-src 'self' wss:; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
};

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (
      url.pathname === "/wrangler.jsonc" ||
      url.pathname.startsWith("/src/")
    ) {
      return new Response("Not found\n", {
        status: 404,
        headers: SECURITY_HEADERS,
      });
    }

    if (SESSION_PATH_PATTERN.test(url.pathname)) {
      const chatUrl = new URL("/chat", url);
      const response = await env.ASSETS.fetch(new Request(chatUrl, request));
      return withSecurityHeaders(response);
    }

    const response = await env.ASSETS.fetch(request);
    return withSecurityHeaders(response);
  },
};

function withSecurityHeaders(response) {
  const headers = new Headers(response.headers);
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
    headers.set(name, value);
  }
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}
