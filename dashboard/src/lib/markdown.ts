// Markdown for wiki pages (§15.1): wikilinks, artifact embeds, and safe output.
//
// Wiki bodies come from agents, so they are untrusted. The renderer escapes
// raw HTML and allows only safe URL schemes. HTML artifacts render only in a
// sandboxed iframe on the artifact origin.

import { Marked, type Tokens } from "marked";

export type WikiTarget =
  | { kind: "page"; project: string; path: string }
  | { kind: "card"; project: string; id: string }
  | { kind: "memory"; project: string; id: string }
  | { kind: "version"; project: string; name: string };

export interface ResolveContext {
  project: string;
  /** Known page paths in the current project, to mark missing links. */
  pages?: Set<string>;
  /** Known version names in the current project. */
  versions?: Set<string>;
  /** Card prefixes that exist, for example `TER`, `WEB`. */
  cardPrefixes?: Set<string>;
  /** Map card prefix to its project. */
  prefixProject?: Record<string, string>;
}

const CARD_RE = /^([A-Z][A-Z0-9]{1,9})-(\d+)$/;
const MEM_RE = /^MEM-\d+$/;
const SEMVER_RE = /^v\d+\.\d+(\.\d+)?([-+].*)?$/;

/** Resolve the inside of `[[...]]` to a target. */
export function resolveWikilink(raw: string, ctx: ResolveContext): WikiTarget {
  const ref = raw.trim();
  if (ref.startsWith("project:")) {
    const rest = ref.slice("project:".length);
    const slash = rest.indexOf("/");
    if (slash > 0) return { kind: "page", project: rest.slice(0, slash), path: rest.slice(slash + 1) };
    return { kind: "page", project: rest, path: "index" };
  }
  if (MEM_RE.test(ref)) return { kind: "memory", project: ctx.project, id: ref };
  const card = CARD_RE.exec(ref);
  if (card && (!ctx.cardPrefixes || ctx.cardPrefixes.has(card[1]!))) {
    return { kind: "card", project: ctx.prefixProject?.[card[1]!] ?? ctx.project, id: ref };
  }
  if (ctx.versions?.has(ref) || SEMVER_RE.test(ref)) return { kind: "version", project: ctx.project, name: ref };
  return { kind: "page", project: ctx.project, path: ref };
}

export function targetHref(t: WikiTarget): string {
  const p = `/p/${encodeURIComponent(t.project)}`;
  switch (t.kind) {
    case "page":
      return `${p}/knowledge/wiki/${t.path.split("/").map(encodeURIComponent).join("/")}`;
    case "card":
      return `${p}/board?card=${encodeURIComponent(t.id)}`;
    case "memory":
      return `${p}/knowledge/memory?id=${encodeURIComponent(t.id)}`;
    case "version":
      return `${p}/board/versions/${encodeURIComponent(t.name)}`;
  }
}

export interface FrontMatter {
  status?: string;
  reviewer?: string;
  [key: string]: string | undefined;
}

/** Split simple `key: value` front matter from a body. */
export function splitFrontMatter(src: string): { meta: FrontMatter; body: string } {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(src);
  if (!m) return { meta: {}, body: src };
  const meta: FrontMatter = {};
  for (const line of m[1]!.split(/\r?\n/)) {
    const kv = /^([\w-]+):\s*(.*)$/.exec(line);
    if (kv) meta[kv[1]!] = kv[2]!.trim();
  }
  return { meta, body: src.slice(m[0].length) };
}

const SAFE_URL = /^(https?:|mailto:|\/|#|\.|artifact:|data:image\/(png|gif|jpeg|webp);)/i;

export function safeUrl(href: string): string | null {
  const h = href.trim();
  if (!h) return null;
  // Relative paths with no scheme are fine.
  if (!/^[a-z][a-z0-9+.-]*:/i.test(h)) return h;
  return SAFE_URL.test(h) ? h : null;
}

const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

export interface RenderOptions extends ResolveContext {
  /** Turn an artifact ID into a URL (a signed URL in real mode). */
  artifactUrl?: (id: string) => string | undefined;
}

/** The longest query of a `db` block. The sanitizer drops a longer attribute. */
export const DB_QUERY_MAX = 2000;

/** The query of a `db` block: its lines joined with spaces. */
export const dbQueryText = (text: string): string => text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean).join(" ").slice(0, DB_QUERY_MAX);

/**
 * Render a wiki body to HTML. Wikilinks become `<a class="wikilink">`.
 * Artifact fences become placeholders `<div data-artifact="ART-n">` that the
 * page component replaces with a sandboxed viewer. A `db` fence becomes
 * `<div class="db-embed" data-db-query="...">`; the page component runs the
 * query and shows the answer as text.
 */
export function renderWiki(src: string, opts: RenderOptions): string {
  const { body } = splitFrontMatter(src);
  const md = new Marked({ gfm: true, breaks: false });

  md.use({
    extensions: [
      {
        name: "wikilink",
        level: "inline",
        start: (s: string) => s.indexOf("[["),
        tokenizer(s: string) {
          const m = /^\[\[([^\]\n]+)\]\]/.exec(s);
          if (!m) return undefined;
          return { type: "wikilink", raw: m[0], ref: m[1]! };
        },
        renderer(token) {
          const ref = String((token as unknown as { ref: string }).ref);
          const t = resolveWikilink(ref, opts);
          const missing = t.kind === "page" && t.project === opts.project && opts.pages && !opts.pages.has(t.path);
          return `<a class="wikilink wikilink-${t.kind}${missing ? " missing" : ""}" href="${esc(targetHref(t))}">${esc(ref)}</a>`;
        },
      },
    ],
    renderer: {
      html(token: Tokens.HTML | Tokens.Tag) {
        return esc(token.text);
      },
      code(token: Tokens.Code) {
        const lang = (token.lang ?? "").trim();
        const art = /^artifact\s+(ART-\d+)/.exec(lang) ?? (lang === "artifact" ? /(ART-\d+)/.exec(token.text) : null);
        if (art) return `<div class="artifact-embed" data-artifact="${esc(art[1]!)}"></div>\n`;
        // A DB query block (docs/api.md §27): the component reads the query as text from the attribute.
        if (lang === "db") return `<div class="db-embed" data-db-query="${esc(dbQueryText(token.text))}"></div>\n`;
        const cls = lang ? ` class="language-${esc(lang.split(/\s/)[0]!)}"` : "";
        return `<pre><code${cls}>${esc(token.text)}</code></pre>\n`;
      },
      link(token: Tokens.Link) {
        const text = this.parser.parseInline(token.tokens);
        const href = safeUrl(token.href);
        if (!href) return text;
        const art = /^artifact:(ART-\d+)$/.exec(href);
        const url = art ? (opts.artifactUrl?.(art[1]!) ?? "#") : href;
        const external = /^https?:/i.test(url);
        return `<a href="${esc(url)}"${token.title ? ` title="${esc(token.title)}"` : ""}${external ? ' target="_blank" rel="noopener noreferrer"' : ""}${art ? ` data-artifact-link="${art[1]}"` : ""}>${text}</a>`;
      },
      image(token: Tokens.Image) {
        const href = safeUrl(token.href);
        if (!href) return esc(token.text);
        const art = /^artifact:(ART-\d+)$/.exec(href);
        const url = art ? opts.artifactUrl?.(art[1]!) : href;
        if (!url) return `<span class="missing">[missing ${esc(href)}]</span>`;
        return `<img src="${esc(url)}" alt="${esc(token.text)}" loading="lazy">`;
      },
    },
  });

  return md.parse(body, { async: false });
}

/** Render trusted-shape Markdown (artifacts, notes) with the same safety rules. */
export const renderMarkdown = (src: string, project = ""): string => renderWiki(src, { project });
