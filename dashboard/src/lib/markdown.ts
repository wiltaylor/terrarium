// Markdown for wiki pages (§15.1): wikilinks, artifact embeds, and safe output.
//
// Wiki bodies come from agents, so they are untrusted. The renderer escapes
// raw HTML and allows only safe URL schemes. HTML artifacts render only in a
// sandboxed iframe on the artifact origin.

import { Marked, type Tokens } from "marked";

export type WikiTarget =
  | { kind: "page"; project: string; path: string }
  | { kind: "card"; project: string; id: string }
  | { kind: "memory"; project: string; id: string };

export interface ResolveContext {
  project: string;
  /** Known page paths in the current project, to mark missing links. */
  pages?: Set<string>;
  /** Card prefixes that exist, for example `TER`, `WEB`. */
  cardPrefixes?: Set<string>;
  /** Map card prefix to its project. */
  prefixProject?: Record<string, string>;
}

const CARD_RE = /^([A-Z][A-Z0-9]{1,9})-(\d+)$/;
const MEM_RE = /^MEM-\d+$/;

/** Resolve the inside of `[[...]]` to a target. A version name is a page since TER-58 (versions are card labels), as in the daemon. */
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
  return { kind: "page", project: ctx.project, path: ref };
}

/** A path part that is not empty, `.`, or `..`. */
const safePart = (s: string): boolean => s.trim() !== "" && s !== "." && s !== "..";

/**
 * A page target that stays inside `/p/<project>/knowledge/wiki/` (TER-598):
 * a project with no `/`, and a project and page path with no empty, `.`, or
 * `..` part. A text or
 * `wiki:` link refuses any other page.
 */
export const safePageTarget = (t: WikiTarget): boolean => t.kind === "page" && safePart(t.project) && !t.project.includes("/") && t.path.split("/").every(safePart);

export function targetHref(t: WikiTarget): string {
  const p = `/p/${encodeURIComponent(t.project)}`;
  switch (t.kind) {
    case "page":
      return `${p}/knowledge/wiki/${t.path.split("/").map(encodeURIComponent).join("/")}`;
    case "card":
      return `${p}/board?card=${encodeURIComponent(t.id)}`;
    case "memory":
      return `${p}/knowledge/memory?id=${encodeURIComponent(t.id)}`;
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

/** True when an http(s) or protocol-relative URL carries a user name or password (`https://user:pass@host`). */
function hasUserInfo(h: string): boolean {
  if (!/^(?:https?:|[\\/]{2})/i.test(h)) return false;
  try {
    // Without a base, `https:user@host` parses as on a page with another scheme: with the user.
    const u = /^https?:/i.test(h) ? new URL(h) : new URL(h, "https://base.invalid/");
    return u.username !== "" || u.password !== "";
  } catch {
    return true;
  }
}

/** The URL when its scheme is safe, or null. An http(s) URL with a user name or password is refused, as in `LinkText` (TER-598). */
export function safeUrl(href: string): string | null {
  const h = href.trim();
  if (!h) return null;
  if (hasUserInfo(h)) return null;
  // Relative paths with no scheme are fine.
  if (!/^[a-z][a-z0-9+.-]*:/i.test(h)) return h;
  return SAFE_URL.test(h) ? h : null;
}

const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

export interface RenderOptions extends ResolveContext {
  /** Card and memory text keeps single newlines and does not strip front matter. */
  textBody?: boolean;
  /** Turn an artifact ID into a URL (a signed URL in real mode). */
  artifactUrl?: (id: string) => string | undefined;
  /**
   * Card prefix to project, for the projects of the reader (TER-571). With it,
   * a bare card key such as `TER-12` and a `[text](card:TER-12)` link become
   * card links (`wikilink-card`), and `[text](wiki:<page>)` a page link.
   */
  cardKeys?: Record<string, string>;
}

/** A bare card key at the start of `s`, with no letter, digit, or `_` after it. */
const CARD_KEY_AT = /^([A-Z][A-Z0-9]{1,9})-\d{1,9}(?![A-Za-z0-9_])/;
/** The first bare card key in `s` with no letter, digit, `_`, `-`, or `/` before it. */
const CARD_KEY_START = /(?<![A-Za-z0-9_/-])[A-Z][A-Z0-9]{1,9}-\d{1,9}(?![A-Za-z0-9_])/g;

/** The project of a card key of the reader, or null. */
const keyProject = (key: string, keys: Record<string, string> | undefined): string | null => {
  const m = CARD_KEY_AT.exec(key);
  if (!keys || !m || m[0] !== key || !Object.hasOwn(keys, m[1]!)) return null;
  return keys[m[1]!] ?? null;
};

/** The in-app href of `wiki:<page>` or `card:<KEY>`, or null (a refused link renders as its text). */
function appLinkHref(href: string, opts: RenderOptions): { href: string; card: boolean } | null {
  if (/^card:/i.test(href)) {
    const id = href.slice("card:".length);
    const project = keyProject(id, opts.cardKeys);
    return project ? { href: targetHref({ kind: "card", project, id }), card: true } : null;
  }
  let page: string;
  try {
    page = decodeURIComponent(href.slice("wiki:".length)).trim();
  } catch {
    return null;
  }
  const t = resolveWikilink(page, { project: opts.project, cardPrefixes: new Set() });
  if (!safePageTarget(t)) return null;
  return { href: targetHref(t), card: false };
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
  const body = opts.textBody ? src : splitFrontMatter(src).body;
  const md = new Marked({ gfm: true, breaks: opts.textBody ?? false });

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
      ...(opts.cardKeys ? [cardKeyExtension(opts.cardKeys)] : []),
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
        if (opts.cardKeys && /^(?:wiki|card):/i.test(token.href.trim())) {
          const app = appLinkHref(token.href.trim(), opts);
          if (!app) return text;
          return `<a class="wikilink wikilink-${app.card ? "card" : "page"}" href="${esc(app.href)}">${text}</a>`;
        }
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

/** A bare card key of the reader's projects: a `wikilink-card` link. Not inside a link (no `<a>` in an `<a>`). */
function cardKeyExtension(keys: Record<string, string>) {
  return {
    name: "cardkey",
    level: "inline" as const,
    start: (s: string) => {
      for (const m of s.matchAll(CARD_KEY_START)) if (keyProject(m[0], keys)) return m.index;
      return undefined;
    },
    tokenizer(this: { lexer: { state: { inLink: boolean } } }, s: string, tokens: { raw: string }[]) {
      if (this.lexer.state.inLink) return undefined;
      // The tokenizer sees only the rest of the text: the key must not go on a word of the token before it.
      if (/[A-Za-z0-9_/-]$/.test(tokens.at(-1)?.raw ?? "")) return undefined;
      const m = CARD_KEY_AT.exec(s);
      if (!m || !keyProject(m[0], keys)) return undefined;
      return { type: "cardkey", raw: m[0], id: m[0] };
    },
    renderer(token: { id?: unknown }) {
      const id = String(token.id);
      const project = keyProject(id, keys)!;
      return `<a class="wikilink wikilink-card" href="${esc(targetHref({ kind: "card", project, id }))}">${esc(id)}</a>`;
    },
  };
}

/** Render trusted-shape Markdown (artifacts, notes) with the same safety rules. `extra` adds options such as `cardKeys`. */
export const renderMarkdown = (src: string, project = "", extra: Omit<RenderOptions, "project"> = {}): string => renderWiki(src, { ...extra, project });

/** The card of a `wikilink-card` href (`/p/<project>/board?card=<id>`), or null. */
export function cardOfHref(href: string): { project: string; id: string } | null {
  const m = /^\/p\/([^/?#]+)\/board\?card=([^&#]+)$/.exec(href);
  if (!m) return null;
  try {
    return { project: decodeURIComponent(m[1]!), id: decodeURIComponent(m[2]!) };
  } catch {
    return null;
  }
}
