import { describe, expect, it } from "vitest";

import {
  decodeEntities,
  extractReadableText,
  isPrivateIp,
  normalizeDuckDuckGoUrl,
  parseDuckDuckGoHtml,
} from "./web-tools";

describe("isPrivateIp", () => {
  it("blocks private and special IPv4 ranges", () => {
    for (const ip of [
      "127.0.0.1",
      "10.1.2.3",
      "172.16.0.1",
      "172.31.255.255",
      "192.168.1.1",
      "169.254.169.254",
      "100.64.0.1",
      "0.0.0.0",
      "198.18.0.1",
      "224.0.0.1",
      "255.255.255.255",
    ]) {
      expect(isPrivateIp(ip), ip).toBe(true);
    }
  });

  it("allows public IPv4 addresses", () => {
    for (const ip of ["8.8.8.8", "1.1.1.1", "93.184.216.34", "172.32.0.1"]) {
      expect(isPrivateIp(ip), ip).toBe(false);
    }
  });

  it("blocks private and special IPv6 ranges", () => {
    for (const ip of [
      "::1",
      "::",
      "fc00::1",
      "fd12:3456::1",
      "fe80::1",
      "ff02::1",
      "::ffff:10.0.0.1",
      "::ffff:127.0.0.1",
    ]) {
      expect(isPrivateIp(ip), ip).toBe(true);
    }
  });

  it("allows public IPv6 addresses", () => {
    for (const ip of ["2606:4700::1111", "2001:4860:4860::8888"]) {
      expect(isPrivateIp(ip), ip).toBe(false);
    }
  });

  it("fails closed on non-IP input", () => {
    expect(isPrivateIp("not-an-ip")).toBe(true);
  });
});

describe("normalizeDuckDuckGoUrl", () => {
  it("decodes the uddg redirect parameter", () => {
    const href =
      "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
    expect(normalizeDuckDuckGoUrl(href)).toBe("https://example.com/page");
  });

  it("passes through direct http urls", () => {
    expect(normalizeDuckDuckGoUrl("https://example.com/x")).toBe(
      "https://example.com/x",
    );
  });

  it("drops internal duckduckgo links and empty hrefs", () => {
    expect(normalizeDuckDuckGoUrl("https://duckduckgo.com/y.js?ad=1")).toBe(
      null,
    );
    expect(normalizeDuckDuckGoUrl("")).toBe(null);
  });
});

describe("parseDuckDuckGoHtml", () => {
  const fixture = `
    <div class="result results_links results_links_deep web-result">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone&rut=1">First <b>Result</b></a>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone">Snippet one &amp; more</a>
    </div>
    <div class="result">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftwo&rut=2">Second Result</a>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftwo">Snippet two</a>
    </div>
  `;

  it("extracts paired titles, urls, and snippets", () => {
    const results = parseDuckDuckGoHtml(fixture, 10);
    expect(results).toEqual([
      {
        title: "First Result",
        url: "https://example.com/one",
        snippet: "Snippet one & more",
      },
      {
        title: "Second Result",
        url: "https://example.org/two",
        snippet: "Snippet two",
      },
    ]);
  });

  it("respects the result limit", () => {
    expect(parseDuckDuckGoHtml(fixture, 1)).toHaveLength(1);
  });

  it("returns empty for unrecognized markup", () => {
    expect(parseDuckDuckGoHtml("<html><body>captcha</body></html>", 5)).toEqual(
      [],
    );
  });
});

describe("extractReadableText", () => {
  it("extracts title, headings, links, and body text", () => {
    const html = `
      <html>
        <head><title>Page &amp; Title</title><style>body { color: red; }</style></head>
        <body>
          <script>var tracked = true;</script>
          <nav><a href="https://example.com/nav">Nav link</a></nav>
          <h1>Main Heading</h1>
          <p>Hello <b>world</b>, see <a href="https://example.com/doc">the docs</a>.</p>
          <ul><li>Alpha</li><li>Beta</li></ul>
        </body>
      </html>
    `;
    const { title, text } = extractReadableText(html);
    expect(title).toBe("Page & Title");
    expect(text).toContain("# Main Heading");
    expect(text).toContain("Hello world");
    expect(text).toContain("[the docs](https://example.com/doc)");
    expect(text).toContain("- Alpha");
    expect(text).not.toContain("tracked");
    expect(text).not.toContain("color: red");
    expect(text).not.toContain("Nav link");
  });
});

describe("decodeEntities", () => {
  it("decodes named and numeric entities", () => {
    expect(decodeEntities("a &amp; b &lt;c&gt; &#39;d&#39; &#x41;")).toBe(
      "a & b <c> 'd' A",
    );
  });
});
