import { createContext, useContext, useEffect, useRef, useState } from "react";
import type { Artifact } from "../api/protocol";
import { decodeBytes, formatTime } from "../lib/rendering";
import { ImageLightbox, MarkdownContent } from "./Markdown";
import { JsonPreview, TextPreview } from "./JsonPreview";

export interface ArtifactRef {
  artifactId: string;
  version?: number | null;
  path?: string | null;
}

interface ArtifactLoader {
  load: (artifactId: string, version?: number | null) => Promise<Artifact>;
}

// The transcript provides the read seam so the viewer never reaches for a client
// itself; loading stays read-only (`conversation_read_artifact`) and cached.
export const ArtifactContext = createContext<ArtifactLoader | null>(null);

type LoadState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "loaded"; artifact: Artifact }
  | { status: "error"; message: string };

export function ArtifactView({
  artifactId,
  version,
  path,
  triggerLabel,
}: ArtifactRef & { triggerLabel?: string }) {
  const loader = useContext(ArtifactContext);
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<LoadState>({ status: "idle" });

  function toggle() {
    const next = !open;
    setOpen(next);
    if (
      !next ||
      !loader ||
      state.status === "loading" ||
      state.status === "loaded"
    ) {
      return;
    }
    setState({ status: "loading" });
    loader
      .load(artifactId, version ?? null)
      .then((artifact) => setState({ status: "loaded", artifact }))
      .catch((caught) =>
        setState({
          status: "error",
          message: caught instanceof Error ? caught.message : String(caught),
        }),
      );
  }

  return (
    <div className="artifact-view">
      <button
        className="text-button artifact-trigger"
        onClick={toggle}
        type="button"
      >
        <span>{open ? "hide" : (triggerLabel ?? "view artifact")}</span>
        {path && triggerLabel == null ? <code>{path}</code> : null}
      </button>
      {open ? (
        <div className="artifact-body">
          {state.status === "loading" ? (
            <div className="artifact-loading">loading…</div>
          ) : null}
          {state.status === "error" ? (
            <div className="artifact-error">{state.message}</div>
          ) : null}
          {state.status === "loaded" ? (
            <ArtifactContent artifact={state.artifact} />
          ) : null}
          {loader == null ? (
            <div className="artifact-error">artifact unavailable</div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

export function ArtifactWrittenCard({
  artifactId,
  path,
  version,
  createdAt,
}: {
  artifactId: string;
  path: string;
  version: number;
  createdAt: string;
}) {
  return (
    <section className="conversation-event tool-thread">
      <article className="artifact-card">
        <div className="artifact-head">
          <span
            className="tool-glyph artifact-glyph"
            data-filetype={fileTypeKind(path)}
            aria-hidden="true"
          >
            <FileTypeGlyph path={path} />
          </span>
          <span className="artifact-head-text">
            <code>artifact</code>
            <span className="artifact-path">{path}</span>
          </span>
          <span className="artifact-version">v{version}</span>
          <time>{formatTime(createdAt)}</time>
        </div>
        <div className="artifact-foot">
          <ArtifactView
            artifactId={artifactId}
            path={path}
            triggerLabel="view"
            version={version}
          />
        </div>
      </article>
    </section>
  );
}

// A compacted tool result keeps a pointer to the real bytes; find it so the
// inspector can offer to open them instead of showing only a reference.
export function findArtifactRef(value: unknown, depth = 0): ArtifactRef | null {
  if (!value || typeof value !== "object" || depth > 4) {
    return null;
  }
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = findArtifactRef(item, depth + 1);
      if (found) {
        return found;
      }
    }
    return null;
  }

  const record = value as Record<string, unknown>;
  const id = record.artifact_id;
  if (typeof id === "string" && id.length > 0) {
    return {
      artifactId: id,
      version: typeof record.version === "number" ? record.version : null,
      path: typeof record.path === "string" ? record.path : null,
    };
  }

  for (const entry of Object.values(record)) {
    const found = findArtifactRef(entry, depth + 1);
    if (found) {
      return found;
    }
  }
  return null;
}

function ArtifactContent({ artifact }: { artifact: Artifact }) {
  const kind = classifyArtifact(artifact);

  if (kind === "image") {
    const dataUrl = `data:${imageMime(artifact)};base64,${bytesToBase64(artifact.contents)}`;
    return <ImageLightbox alt={artifact.path} src={dataUrl} />;
  }

  if (kind === "video") {
    const dataUrl = `data:${mediaMime(artifact, "video")};base64,${bytesToBase64(artifact.contents)}`;
    return <video className="md-media" controls src={dataUrl} />;
  }

  if (kind === "audio") {
    const dataUrl = `data:${mediaMime(artifact, "audio")};base64,${bytesToBase64(artifact.contents)}`;
    return <audio className="md-audio" controls src={dataUrl} />;
  }

  if (kind === "pdf") {
    return <PdfPreview bytes={artifact.contents} />;
  }

  const text = decodeBytes(artifact.contents);

  if (kind === "html") {
    // Render in a fully locked-down iframe: `sandbox` with no allow-* tokens
    // means no scripts, no forms, no same-origin, so agent-authored HTML can be
    // previewed without it running anything.
    return (
      <iframe
        className="md-html"
        sandbox=""
        srcDoc={text}
        title={artifact.path}
      />
    );
  }

  if (kind === "json") {
    try {
      return (
        <JsonPreview defaultOpen label="artifact" value={JSON.parse(text)} />
      );
    } catch {
      return <TextPreview text={text} />;
    }
  }

  if (kind === "markdown") {
    return <MarkdownContent text={text} />;
  }

  return <TextPreview text={text} />;
}

// Cap on rendered pages so a huge PDF cannot lock the tab; the rest is a note.
const PDF_PREVIEW_PAGE_LIMIT = 10;

// pdf.js is heavy (~hundreds of KB plus a worker), so it is imported on demand
// the first time a PDF is opened rather than shipped in the main bundle.
function PdfPreview({ bytes }: { bytes: number[] }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">(
    "loading",
  );
  const [error, setError] = useState<string | null>(null);
  const [pageCount, setPageCount] = useState(0);

  useEffect(() => {
    let cancelled = false;
    const container = containerRef.current;
    if (!container) {
      return;
    }
    setStatus("loading");
    setError(null);

    void (async () => {
      try {
        const pdfjs = await import("pdfjs-dist");
        const workerUrl = (
          await import("pdfjs-dist/build/pdf.worker.min.mjs?url")
        ).default;
        pdfjs.GlobalWorkerOptions.workerSrc = workerUrl;

        // Copy into a fresh buffer; pdf.js takes ownership of the bytes it reads.
        const doc = await pdfjs.getDocument({ data: Uint8Array.from(bytes) })
          .promise;
        if (cancelled) {
          return;
        }
        setPageCount(doc.numPages);
        container.replaceChildren();
        const pages = Math.min(doc.numPages, PDF_PREVIEW_PAGE_LIMIT);
        for (let n = 1; n <= pages; n += 1) {
          const page = await doc.getPage(n);
          if (cancelled) {
            return;
          }
          const viewport = page.getViewport({ scale: 1.4 });
          const canvas = document.createElement("canvas");
          canvas.className = "md-pdf-page";
          canvas.width = Math.ceil(viewport.width);
          canvas.height = Math.ceil(viewport.height);
          container.appendChild(canvas);
          await page.render({ canvas, viewport }).promise;
        }
        if (!cancelled) {
          setStatus("ready");
        }
      } catch (caught) {
        if (!cancelled) {
          setStatus("error");
          setError(caught instanceof Error ? caught.message : String(caught));
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [bytes]);

  return (
    <div className="md-pdf">
      {status === "loading" ? (
        <div className="md-pdf-note">Loading PDF…</div>
      ) : null}
      {status === "error" ? (
        <div className="md-pdf-note md-pdf-error">
          PDF preview failed: {error}
        </div>
      ) : null}
      <div ref={containerRef} className="md-pdf-pages" />
      {status === "ready" && pageCount > PDF_PREVIEW_PAGE_LIMIT ? (
        <div className="md-pdf-note">
          Showing the first {PDF_PREVIEW_PAGE_LIMIT} of {pageCount} pages.
        </div>
      ) : null}
    </div>
  );
}

type ArtifactKind =
  | "image"
  | "video"
  | "audio"
  | "pdf"
  | "html"
  | "json"
  | "markdown"
  | "text";

const IMAGE_EXT = new Set([
  "png",
  "jpg",
  "jpeg",
  "gif",
  "webp",
  "bmp",
  "ico",
  "svg",
]);
const VIDEO_EXT = new Set(["mp4", "webm", "mov", "m4v", "ogv"]);
const AUDIO_EXT = new Set(["mp3", "wav", "ogg", "oga", "m4a", "aac", "flac"]);

export function classifyArtifact(artifact: Artifact): ArtifactKind {
  const ext = extensionOf(artifact.path);
  if (IMAGE_EXT.has(ext) || sniffImage(artifact.contents)) {
    return "image";
  }
  if (VIDEO_EXT.has(ext)) {
    return "video";
  }
  if (AUDIO_EXT.has(ext)) {
    return "audio";
  }
  if (ext === "pdf" || sniffPdf(artifact.contents)) {
    return "pdf";
  }
  if (ext === "html" || ext === "htm") {
    return "html";
  }
  if (ext === "json") {
    return "json";
  }
  if (ext === "md" || ext === "markdown") {
    return "markdown";
  }
  if (!ext && looksLikeJson(artifact.contents)) {
    return "json";
  }
  return "text";
}

// "%PDF-" magic so a mislabeled or extension-less PDF still previews.
function sniffPdf(bytes: number[]): boolean {
  return (
    bytes.length >= 5 &&
    bytes[0] === 0x25 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x44 &&
    bytes[3] === 0x46 &&
    bytes[4] === 0x2d
  );
}

function extensionOf(path: string): string {
  const name = path.split(/[?#]/)[0];
  const dot = name.lastIndexOf(".");
  return dot === -1 ? "" : name.slice(dot + 1).toLowerCase();
}

function looksLikeJson(bytes: number[]): boolean {
  const head = decodeBytes(bytes.slice(0, 64)).trimStart();
  return head.startsWith("{") || head.startsWith("[");
}

function sniffImage(bytes: number[]): boolean {
  if (
    bytes.length >= 8 &&
    bytes[0] === 0x89 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x4e &&
    bytes[3] === 0x47
  ) {
    return true;
  }
  if (
    bytes.length >= 3 &&
    bytes[0] === 0xff &&
    bytes[1] === 0xd8 &&
    bytes[2] === 0xff
  ) {
    return true;
  }
  if (
    bytes.length >= 3 &&
    bytes[0] === 0x47 &&
    bytes[1] === 0x49 &&
    bytes[2] === 0x46
  ) {
    return true;
  }
  return false;
}

function imageMime(artifact: Artifact): string {
  const ext = extensionOf(artifact.path);
  switch (ext) {
    case "jpg":
    case "jpeg":
      return "image/jpeg";
    case "gif":
      return "image/gif";
    case "webp":
      return "image/webp";
    case "bmp":
      return "image/bmp";
    case "ico":
      return "image/x-icon";
    case "svg":
      return "image/svg+xml";
    default:
      return "image/png";
  }
}

function mediaMime(artifact: Artifact, kind: "video" | "audio"): string {
  switch (extensionOf(artifact.path)) {
    case "mp4":
    case "m4v":
      return "video/mp4";
    case "webm":
      return "video/webm";
    case "mov":
      return "video/quicktime";
    case "ogv":
      return "video/ogg";
    case "mp3":
      return "audio/mpeg";
    case "wav":
      return "audio/wav";
    case "ogg":
    case "oga":
      return "audio/ogg";
    case "m4a":
    case "aac":
      return "audio/mp4";
    case "flac":
      return "audio/flac";
    default:
      return kind === "video" ? "video/mp4" : "audio/mpeg";
  }
}

function bytesToBase64(bytes: number[]): string {
  const array = Uint8Array.from(bytes);
  let binary = "";
  const chunkSize = 0x8000;
  for (let index = 0; index < array.length; index += chunkSize) {
    binary += String.fromCharCode(...array.subarray(index, index + chunkSize));
  }
  return btoa(binary);
}

type FileTypeKind = "image" | "media" | "json" | "markdown" | "code" | "text";

const CODE_EXT = new Set([
  "js",
  "jsx",
  "ts",
  "tsx",
  "mjs",
  "cjs",
  "py",
  "rs",
  "go",
  "rb",
  "java",
  "c",
  "cc",
  "cpp",
  "h",
  "hpp",
  "sh",
  "bash",
  "zsh",
  "css",
  "scss",
  "less",
  "html",
  "htm",
  "xml",
  "yaml",
  "yml",
  "toml",
  "sql",
  "php",
]);

// Pick a coarse file family from the path so each artifact card shows a glyph and
// colour that match what it holds, instead of one generic file icon.
export function fileTypeKind(path: string): FileTypeKind {
  const ext = extensionOf(path);
  if (IMAGE_EXT.has(ext)) {
    return "image";
  }
  if (VIDEO_EXT.has(ext) || AUDIO_EXT.has(ext)) {
    return "media";
  }
  if (ext === "json") {
    return "json";
  }
  if (ext === "md" || ext === "markdown") {
    return "markdown";
  }
  if (CODE_EXT.has(ext)) {
    return "code";
  }
  return "text";
}

function FileTypeGlyph({ path }: { path: string }) {
  switch (fileTypeKind(path)) {
    case "image":
      return (
        <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
          <rect x="2" y="3" width="12" height="10" rx="1.6" />
          <circle cx="5.8" cy="6.3" r="1.1" />
          <path d="M2.6 11.4 6.5 7.7l2.1 2.1L11 7.4l2.4 2.6" />
        </svg>
      );
    case "media":
      return (
        <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
          <circle cx="8" cy="8" r="6" />
          <path d="M6.6 5.5 11 8l-4.4 2.5Z" />
        </svg>
      );
    case "json":
      return (
        <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
          <path d="M6.2 3C5 3 5 4.1 5 5.1c0 1.1-.5 1.8-1.3 2.3.8.5 1.3 1.2 1.3 2.3 0 1 0 2.1 1.2 2.1" />
          <path d="M9.8 3c1.2 0 1.2 1.1 1.2 2.1 0 1.1.5 1.8 1.3 2.3-.8.5-1.3 1.2-1.3 2.3 0 1 0 2.1-1.2 2.1" />
        </svg>
      );
    case "code":
      return (
        <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
          <path d="M5.6 5 2.6 8l3 3" />
          <path d="M10.4 5l3 3-3 3" />
        </svg>
      );
    case "markdown":
      return (
        <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
          <rect x="2" y="3.5" width="12" height="9" rx="1.6" />
          <path d="M4.6 10.4V6.6l1.8 1.9 1.8-1.9v3.8" />
          <path d="M10.8 6.6v3M9.5 8.3l1.3 1.4 1.3-1.4" />
        </svg>
      );
    default:
      return (
        <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
          <path d="M9 1.6H4.2A1.2 1.2 0 0 0 3 2.8v10.4a1.2 1.2 0 0 0 1.2 1.2h7.6a1.2 1.2 0 0 0 1.2-1.2V5.4L9 1.6Z" />
          <path d="M9 1.6V5.4h3.8" />
          <path d="M5.4 9h5.2M5.4 11h3.2" />
        </svg>
      );
  }
}
