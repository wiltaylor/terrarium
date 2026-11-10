// Links in plain text (GitHub #31, TER-571): question text, card bodies, and
// card comments.
//
// The text is the words of an agent, so it is untrusted. This module never
// makes HTML: it splits the text into segments, and `components/LinkText`
// renders each one as a text node or an `<a>` element with Solid. The syntax:
//
// - `[text](https://...)` and a bare `http(s)://...` URL: an external link
//   (a new tab, `rel="noopener noreferrer"`). Only http and https: any other
//   scheme (`javascript:`, `data:`, ...) stays plain text.
// - `[text](wiki:<page>)` and `[[page]]`: the wiki page of the project.
// - `[text](card:ABC-12)` and a bare `ABC-12`: the card, in the side panel.
//   Only a prefix of a project that the reader can see (`prefixProject`)
//   makes a link. Any other key stays plain text.

import { resolveWikilink, targetHref } from "./markdown";

export type LinkSegment =
  | { kind: "text"; text: string }
  | { kind: "external"; text: string; href: string }
  | { kind: "wiki"; text: string; href: string }
  | { kind: "card"; text: string; href: string; project: string; id: string };

export interface LinkContext {
  /** The project of the text: wiki links without a project go there. */
  project: string;
  /** Card prefix to project, for the projects of the reader (`projects.prefixes()`). */
  prefixProject: Record<string, string>;
}

const CARD_KEY = /^([A-Z][A-Z0-9]{1,9})-(\d{1,9})$/;
/** The longest URL or label that makes a link. A longer one stays text. */
const MAX_LINK = 2048;

/**
 * One pass over the text. In order: a Markdown link, a wikilink, a bare URL,
 * a bare card key. The card key needs no letter, digit, `-`, `_`, or `/`
 * next to it, so `XTER-1`, `TER-1a`, and `a/TER-1` stay text.
 */
const TOKEN = /\[([^\]\n]{1,512})\]\(([^()\s]{1,2048})\)|\[\[([^\]\n]{1,512})\]\]|\bhttps?:\/\/[^\s<>"'`]+|(?<![A-Za-z0-9_/-])[A-Z][A-Z0-9]{1,9}-\d{1,9}(?![A-Za-z0-9_])/g;

/** An http or https URL in its normal form, or null for any other scheme or a bad URL. */
export function externalHref(raw: string): string | null {
  if (raw.length > MAX_LINK || !/^https?:\/\//i.test(raw)) return null;
  try {
    const u = new URL(raw);
    if (u.protocol !== "http:" && u.protocol !== "https:") return null;
    if (u.username || u.password) return null;
    return u.href;
  } catch {
    return null;
  }
}

/** The punctuation at the end of a bare URL that ends the sentence, not the URL. */
function trimUrl(url: string): string {
  let u = url;
  for (;;) {
    const last = u.at(-1);
    if (!last) return u;
    if (".,;:!?'\"]}".includes(last)) u = u.slice(0, -1);
    // Keep a `)` that closes a `(` in the URL (Wikipedia style), drop the one of the sentence.
    else if (last === ")" && (u.match(/\(/g)?.length ?? 0) < (u.match(/\)/g)?.length ?? 0)) u = u.slice(0, -1);
    else return u;
  }
}

/** A card key of a project of the reader, or null. */
function cardTarget(key: string, ctx: LinkContext): { project: string; id: string; href: string } | null {
  const m = CARD_KEY.exec(key);
  if (!m) return null;
  const project = Object.hasOwn(ctx.prefixProject, m[1]!) ? ctx.prefixProject[m[1]!] : undefined;
  if (!project) return null;
  return { project, id: key, href: targetHref({ kind: "card", project, id: key }) };
}

/** A wiki page path with no empty, `.`, or `..` segment. */
const safePagePath = (path: string): boolean => path.length > 0 && path.split("/").every((s) => s.trim() !== "" && s !== "." && s !== "..");

/** The wiki target of `wiki:<page>` or `[[page]]`. A card key in `[[...]]` is a card, as in the wiki. */
function wikiTarget(ref: string, ctx: LinkContext): LinkSegment | null {
  const page = ref.trim();
  if (!page) return null;
  const card = cardTarget(page, ctx);
  if (card) return { kind: "card", text: page, ...card };
  const t = resolveWikilink(page, { project: ctx.project, cardPrefixes: new Set() });
  if (t.kind === "memory") return { kind: "wiki", text: page, href: targetHref(t) };
  if (t.kind !== "page" || !t.project || !safePagePath(t.path)) return null;
  return { kind: "wiki", text: page, href: targetHref(t) };
}

/** The target of `[label](href)`, or null when the scheme is not allowed. */
function markdownLink(label: string, href: string, ctx: LinkContext): LinkSegment | null {
  if (/^wiki:/i.test(href)) {
    let page = href.slice("wiki:".length);
    try {
      page = decodeURIComponent(page);
    } catch {
      return null;
    }
    const w = wikiTarget(page, ctx);
    return w ? { ...w, text: label } : null;
  }
  if (/^card:/i.test(href)) {
    const card = cardTarget(href.slice("card:".length), ctx);
    return card ? { kind: "card", text: label, ...card } : null;
  }
  const ext = externalHref(href);
  return ext ? { kind: "external", text: label, href: ext } : null;
}

/** Split `text` into text and link segments. Joined, the segment texts give back the text, except for the Markdown syntax of a link. */
export function linkSegments(text: string, ctx: LinkContext): LinkSegment[] {
  const out: LinkSegment[] = [];
  const push = (s: LinkSegment) => {
    const prev = out.at(-1);
    if (s.kind === "text" && prev?.kind === "text") prev.text += s.text;
    else if (s.kind !== "text" || s.text) out.push(s);
  };
  let at = 0;
  for (const m of text.matchAll(TOKEN)) {
    const start = m.index;
    let raw = m[0];
    let seg: LinkSegment | null;
    if (m[1] !== undefined) seg = markdownLink(m[1], m[2]!, ctx);
    else if (m[3] !== undefined) seg = wikiTarget(m[3], ctx);
    else if (/^https?:/i.test(raw)) {
      raw = trimUrl(raw);
      const href = externalHref(raw);
      seg = href ? { kind: "external", text: raw, href } : null;
    } else {
      const card = cardTarget(raw, ctx);
      seg = card ? { kind: "card", text: raw, ...card } : null;
    }
    if (!seg) continue;
    push({ kind: "text", text: text.slice(at, start) });
    push(seg);
    at = start + raw.length;
  }
  push({ kind: "text", text: text.slice(at) });
  return out;
}
