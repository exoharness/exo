import {
  isValidElement,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeHighlight from "rehype-highlight";
import rehypeKatex from "rehype-katex";
import "katex/dist/katex.min.css";
import { CopyButton } from "./CopyButton";

const VIDEO_EXT = new Set(["mp4", "webm", "mov", "m4v", "ogv"]);
const AUDIO_EXT = new Set(["mp3", "wav", "ogg", "oga", "m4a", "flac"]);

export function MarkdownContent({ text }: { text: string }) {
  const normalized = text.replace(/\r\n/g, "\n").trim();

  if (!normalized) {
    return <p className="markdown-content empty-copy">(empty)</p>;
  }

  return (
    <div className="markdown-content">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={[
          rehypeKatex,
          [rehypeHighlight, { detect: true, ignoreMissing: true }],
        ]}
        components={{
          // Drop react-markdown's `node` prop so it never lands on the DOM as a
          // bogus `node="[object Object]"` attribute.
          a: ({ node: _node, ...props }) => (
            <a {...props} rel="noreferrer noopener" target="_blank" />
          ),
          img: ({ src, alt }) => (
            <MediaEmbed
              alt={typeof alt === "string" ? alt : ""}
              src={typeof src === "string" ? src : ""}
            />
          ),
          pre: ({ children, node: _node, ...props }) => {
            const mermaid = extractMermaid(children);
            if (mermaid !== null) {
              return <MermaidDiagram source={mermaid} />;
            }
            const codeText = extractText(children);
            return (
              <div className="code-block-wrap">
                <CopyButton
                  className="copy-button code-copy-button"
                  text={codeText}
                />
                <pre {...props}>{children}</pre>
              </div>
            );
          },
        }}
      >
        {normalized}
      </ReactMarkdown>
    </div>
  );
}

// A ```mermaid fenced block reaches <pre> as a <code class="language-mermaid">
// child (highlight.js leaves unknown languages untouched). Pull the raw source
// out so it can be rendered as a diagram instead of monospaced text.
function extractMermaid(children: ReactNode): string | null {
  const child = Array.isArray(children) ? children[0] : children;
  if (!isValidElement(child)) {
    return null;
  }
  const props = child.props as { className?: string; children?: ReactNode };
  if (!props.className?.includes("language-mermaid")) {
    return null;
  }
  return extractText(props.children).trim();
}

function extractText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") {
    return "";
  }
  if (typeof node === "string" || typeof node === "number") {
    return String(node);
  }
  if (Array.isArray(node)) {
    return node.map(extractText).join("");
  }
  if (isValidElement(node)) {
    return extractText((node.props as { children?: ReactNode }).children);
  }
  return "";
}

// Mermaid renders asynchronously, but in a virtualized transcript a diagram row
// mounts and unmounts as it scrolls (and during virtuoso's height measurement),
// which used to cancel the render before it ever painted. Caching the rendered
// SVG by (theme, source) at module scope — and letting the render promise outlive
// any single mount — makes a remounted diagram paint instantly from cache instead
// of restarting and getting cancelled every time.
const mermaidSvgCache = new Map<string, string>();
const mermaidPending = new Map<string, Promise<string>>();
let mermaidRenderSeq = 0;

function mermaidCacheKey(source: string, dark: boolean): string {
  return `${dark ? "dark" : "default"}:${source}`;
}

function renderMermaidSvg(source: string, dark: boolean): Promise<string> {
  const key = mermaidCacheKey(source, dark);
  const cached = mermaidSvgCache.get(key);
  if (cached !== undefined) {
    return Promise.resolve(cached);
  }
  let pending = mermaidPending.get(key);
  if (!pending) {
    pending = (async () => {
      const mermaid = (await import("mermaid")).default;
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        theme: dark ? "dark" : "default",
        fontFamily: "inherit",
      });
      mermaidRenderSeq += 1;
      const { svg } = await mermaid.render(
        `mmd-render-${mermaidRenderSeq}`,
        source,
      );
      mermaidSvgCache.set(key, svg);
      mermaidPending.delete(key);
      return svg;
    })();
    mermaidPending.set(key, pending);
  }
  return pending;
}

// Mermaid emits the SVG with width="100%" and an inline max-width (its true pixel
// width). A width:100% inside a shrink-to-fit box collapses to 0, so pin the SVG to
// that concrete max-width — then the bordered box hugs the real diagram size.
function mountMermaidSvg(container: HTMLElement, svg: string) {
  container.innerHTML = svg;
  const el = container.querySelector("svg");
  if (el && el.style.maxWidth) {
    el.style.width = el.style.maxWidth;
  }
}

function MermaidDiagram({ source }: { source: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [themeVersion, setThemeVersion] = useState(0);

  // Re-render the diagram when the app theme flips so its colors stay in sync.
  useEffect(() => {
    const observer = new MutationObserver(() =>
      setThemeVersion((value) => value + 1),
    );
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    let active = true;
    const dark = document.documentElement.getAttribute("data-theme") === "dark";

    const cached = mermaidSvgCache.get(mermaidCacheKey(source, dark));
    if (cached !== undefined) {
      // Synchronous cache hit — paint immediately, no async cancellation window.
      if (ref.current) {
        mountMermaidSvg(ref.current, cached);
      }
      setError(null);
      return;
    }

    renderMermaidSvg(source, dark)
      .then((svg) => {
        if (active && ref.current) {
          // securityLevel "strict" runs mermaid's output through DOMPurify, so the
          // SVG is already sanitized — the documented safe way to mount it.
          mountMermaidSvg(ref.current, svg);
          setError(null);
        }
      })
      .catch((renderError: unknown) => {
        if (active) {
          setError(
            renderError instanceof Error
              ? renderError.message
              : String(renderError),
          );
        }
      });

    return () => {
      active = false;
    };
  }, [source, themeVersion]);

  if (error) {
    return (
      <div className="mermaid-error">
        <span>diagram failed to render</span>
        <pre>
          <code>{source}</code>
        </pre>
      </div>
    );
  }

  return <div aria-label="diagram" className="mermaid-diagram" ref={ref} />;
}

function MediaEmbed({ src, alt }: { src: string; alt: string }) {
  if (!src) {
    return null;
  }

  const ext = src.split(/[?#]/)[0].split(".").pop()?.toLowerCase() ?? "";

  if (VIDEO_EXT.has(ext)) {
    return <video className="md-media" controls src={src} />;
  }

  if (AUDIO_EXT.has(ext)) {
    return <audio className="md-audio" controls src={src} />;
  }

  return <ImageLightbox alt={alt} src={src} />;
}

// The click-to-zoom image used by markdown, reused by the artifact viewer so
// inline images and artifact images share one lightbox behaviour.
export function ImageLightbox({ src, alt }: { src: string; alt: string }) {
  const [zoomed, setZoomed] = useState(false);

  // Let keyboard users dismiss the zoom overlay, not just a pointer click.
  useEffect(() => {
    if (!zoomed) {
      return;
    }
    function onKeyDown(event: globalThis.KeyboardEvent) {
      if (event.key === "Escape") {
        setZoomed(false);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [zoomed]);

  if (!src) {
    return null;
  }

  return (
    <>
      <button
        aria-label={alt ? `Zoom image: ${alt}` : "Zoom image"}
        className="md-image"
        onClick={() => setZoomed(true)}
        type="button"
      >
        <img alt={alt} loading="lazy" src={src} />
      </button>
      {zoomed
        ? createPortal(
            <div
              aria-label={alt ? `Zoomed image: ${alt}` : "Zoomed image"}
              aria-modal="true"
              className="lightbox"
              onClick={() => setZoomed(false)}
              role="dialog"
            >
              <img alt={alt} src={src} />
            </div>,
            document.body,
          )
        : null}
    </>
  );
}
