// DOMPurify behind the markdown renderer (docs/security-review.md F3).
//
// `lib/markdown.ts` already escapes raw HTML and filters URL schemes. This is
// the second, independent layer: every string of untrusted HTML goes through
// `sanitizeToFragment` before it reaches the DOM, and `setSafeHtml` is the one
// sink. ESLint forbids `innerHTML` and its relatives everywhere else.
//
// The config allows only the tags and attributes that the renderer emits. It
// forbids SVG and MathML, every event handler, `style`, and every URL scheme
// but http(s), mailto, and relative paths. An `<img>` may also take a
// base64 PNG, JPEG, or GIF `data:` URL.

import DOMPurify, { type Config, type DOMPurify as Purifier } from "dompurify";

/** The tags `renderWiki` emits (marked with GFM, plus our extensions). */
export const ALLOWED_TAGS = [
  "a", "blockquote", "br", "code", "del", "div", "em", "h1", "h2", "h3", "h4", "h5", "h6",
  "hr", "img", "input", "li", "ol", "p", "pre", "span", "strong",
  "table", "tbody", "td", "th", "thead", "tr", "ul",
] as const;

/** The attributes `renderWiki` emits. Hooks below narrow their values further. */
export const ALLOWED_ATTR = [
  "align", "alt", "checked", "class", "data-artifact", "data-artifact-link", "data-db-query", "disabled",
  "href", "loading", "rel", "src", "start", "target", "title", "type",
] as const;

export const PURIFY_CONFIG: Config = {
  ALLOWED_TAGS: [...ALLOWED_TAGS],
  ALLOWED_ATTR: [...ALLOWED_ATTR],
  // Belt and braces: none of these are in ALLOWED_TAGS, but name them so a
  // future edit to the allowlist cannot let them in by accident.
  FORBID_TAGS: ["svg", "math", "script", "style", "iframe", "frame", "object", "embed", "form", "meta", "base", "link", "template", "noscript", "xmp", "plaintext"],
  FORBID_ATTR: ["style", "srcset", "formaction", "action", "xlink:href", "srcdoc", "ping"],
  ALLOW_DATA_ATTR: false,
  ALLOW_ARIA_ATTR: false,
  ALLOW_UNKNOWN_PROTOCOLS: false,
  ALLOW_SELF_CLOSE_IN_ATTR: false,
  // Only http(s), mailto, and URLs with no scheme. The hook below checks again
  // on a normalised value and adds the image `data:` rule.
  ALLOWED_URI_REGEXP: /^(?:(?:https?|mailto):|[^a-z]|[a-z+.-]+(?:[^a-z+.\-:]|$))/i,
  // The query of a `db` block starts with `type:`, which reads as a URL
  // scheme. It is never a URL: the page reads it as text (the hook below
  // keeps it on a div only, at most 2000 characters).
  ADD_URI_SAFE_ATTR: ["data-db-query"],
  SAFE_FOR_TEMPLATES: false,
  SANITIZE_DOM: true,
  SANITIZE_NAMED_PROPS: true,
  WHOLE_DOCUMENT: false,
  KEEP_CONTENT: true,
};

const SAFE_IMAGE_DATA = /^data:image\/(?:png|jpeg|gif);base64,[a-z0-9+/=\s]*$/i;
const SCHEME = /^([a-z][a-z0-9+.-]*):/i;
const CLASS_TOKEN = /^(?:wikilink|wikilink-(?:page|card|memory|version)|missing|artifact-embed|db-embed|language-[\w+#.-]{1,40})$/;
/** The query of a `db` block (lib/markdown DB_QUERY_MAX). Any text: the page reads it with `dataset`, never as HTML. */
const DB_QUERY_MAX = 2000;
const ARTIFACT_ID = /^ART-\d+$/;
const remoteImages = new WeakMap<Node, string>();
let captureImages = false;
export const remoteImageSource = (node: Node): string | undefined => remoteImages.get(node);

/**
 * Browsers drop ASCII whitespace and C0 controls around a URL and inside its
 * scheme (`java\tscript:`). Check the value the browser will act on.
 */
export function normaliseUrl(value: string): string {
  // eslint-disable-next-line no-control-regex -- matching control characters is the point
  return value.replace(/[\u0000- \u007f-\u009f]/g, "");
}

/** True when `value` is a URL that a link may point to. */
export function isSafeHref(value: string): boolean {
  const v = normaliseUrl(value);
  if (!v) return false;
  if (v.startsWith("//")) return false; // scheme-relative: another host
  if (v.startsWith("\\") || v.startsWith("/\\")) return false;
  const m = SCHEME.exec(v);
  if (!m) return true; // relative path, fragment, or query
  const scheme = m[1]!.toLowerCase();
  return scheme === "http" || scheme === "https" || scheme === "mailto";
}

/** True when `value` is a URL an `<img>` may load. */
export function isSafeImageSrc(value: string): boolean {
  const v = normaliseUrl(value);
  if (/^data:/i.test(v)) return SAFE_IMAGE_DATA.test(value.trim());
  const m = SCHEME.exec(v);
  if (m && !/^https?$/i.test(m[1]!)) return false;
  return isSafeHref(value);
}

const isExternal = (href: string) => /^https?:/i.test(normaliseUrl(href));

function addHooks(p: Purifier) {
  p.addHook("uponSanitizeAttribute", (node, data) => {
    const name = data.attrName;
    const value = data.attrValue;
    const tag = node.nodeName.toLowerCase();
    let keep = true;
    if (name === "href") keep = tag === "a" && isSafeHref(value);
    else if (name === "src") {
      keep = tag === "img" && isSafeImageSrc(value);
      if (captureImages && keep) remoteImages.set(node, value);
    }
    else if (name === "data-artifact" || name === "data-artifact-link") keep = ARTIFACT_ID.test(value);
    else if (name === "data-db-query") keep = tag === "div" && value.length <= DB_QUERY_MAX;
    else if (name === "class") {
      const tokens = value.split(/\s+/).filter((t) => CLASS_TOKEN.test(t));
      data.attrValue = tokens.join(" ");
      keep = tokens.length > 0;
    } else if (name === "type") keep = tag === "input" && value.toLowerCase() === "checkbox";
    else if (name === "checked" || name === "disabled") keep = tag === "input";
    else if (name === "loading") keep = tag === "img" && value === "lazy";
    else if (name === "align") keep = /^(?:left|right|center)$/i.test(value);
    else if (name === "start") keep = tag === "ol" && /^\d{1,9}$/.test(value);
    else if (name === "target" || name === "rel") keep = false; // set again below from the href
    if (!keep) {
      data.keepAttr = false;
      // Stop DOMPurify's own checks from putting it back.
      data.forceKeepAttr = false;
    }
  });

  p.addHook("afterSanitizeAttributes", (node) => {
    const el = node as Element;
    const tag = el.nodeName.toLowerCase();
    if (tag === "a") {
      const href = el.getAttribute("href");
      if (href && isExternal(href)) {
        el.setAttribute("target", "_blank");
        el.setAttribute("rel", "noopener noreferrer");
      } else {
        el.removeAttribute("target");
        el.removeAttribute("rel");
      }
    }
    if (tag === "input") {
      // Only the disabled task-list checkbox that marked emits.
      if (el.getAttribute("type") !== "checkbox") el.remove();
      else el.setAttribute("disabled", "");
    }
  });
}

let instance: Purifier | null = null;

/** One private DOMPurify instance, so our hooks do not leak to other users of the library. */
function purifier(): Purifier {
  if (instance) return instance;
  if (typeof window === "undefined") throw new Error("sanitize: no DOM (run in a browser or a jsdom test)");
  const p = DOMPurify(window);
  if (!p.isSupported) throw new Error("sanitize: DOMPurify is not supported in this browser");
  addHooks(p);
  instance = p;
  return p;
}

/** Sanitize to a fragment. Appending a fragment avoids a second HTML parse (mXSS). */
export function sanitizeToFragment(html: string, blockImages = false): DocumentFragment {
  captureImages = blockImages;
  try {
    return purifier().sanitize(html, { ...PURIFY_CONFIG, FORBID_ATTR: [...PURIFY_CONFIG.FORBID_ATTR!, ...(blockImages ? ["src"] : [])], RETURN_DOM_FRAGMENT: true });
  } finally { captureImages = false; }
}

/** Sanitize to a string. For tests and for places that only compare output. */
export function sanitizeHtml(html: string): string {
  return purifier().sanitize(html, { ...PURIFY_CONFIG, RETURN_DOM_FRAGMENT: false, RETURN_DOM: false }) as string;
}

/** The only sink for untrusted HTML: replace the children of `el` with the sanitized markup. */
export function setSafeHtml(el: Element, html: string, options?: { blockImages?: boolean; transform?: (fragment: DocumentFragment) => void }): void {
  const fragment = sanitizeToFragment(html, options?.blockImages);
  options?.transform?.(fragment);
  el.replaceChildren(fragment);
}
