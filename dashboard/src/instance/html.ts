import { movedTo } from "../app/nav";
import type { Instance } from "./create";
import { remoteImageSource } from "../lib/sanitize";

/** Only the selected remote's origin or relative paths, never arbitrary hosts. */
export function remoteImagePath(raw: string, page: URL, instance: Pick<Instance, "key" | "path" | "remoteOrigin">): string | undefined {
  const name = instance.key.startsWith("remote:") ? instance.key.slice(7) : "";
  if (!/^[a-z][a-z0-9-]{0,31}$/.test(name) || /[\\\s]/.test(raw)) return;
  try {
    const target = new URL(raw, page);
    if (!/^https?:$/.test(target.protocol) || target.username || target.password) return;
    if (target.origin !== page.origin && target.origin !== instance.remoteOrigin) return;
    const path = instance.path(target.pathname);
    if (!/^\/(?:api|a)\//.test(path) || /%(?:2f|5c|25|2e|00)/i.test(path)) return;
    return `/api/remotes/${name}/proxy/image${path}${target.search}`;
  } catch { return; }
}

/** Fail closed for same-origin paths which are not dashboard pages. */
function dashboardPath(path: string): boolean {
  return path === "/" || movedTo(path) !== undefined || [
    /^\/(?:projects|bookmarks|inbox|threads|schedules|calendar)\/?$/,
    /^\/(?:agents|sessions|threads)\/[^/]+\/?$/,
    /^\/logs\/(?:mail|events|audit)\/?$/,
    /^\/settings\/(?:account|subscriptions|secrets|templates|system-prompts|tools|targets|harness|hosts|links|software)\/?$/,
    /^\/settings\/skills(?:\/[^/]+)?\/?$/,
    /^\/p\/[^/]+\/?$/,
    /^\/p\/[^/]+\/(?:dashboard|pipelines|calendar|agents|board|schedules|rules)\/?$/,
    /^\/p\/[^/]+\/board\/(?:done|artifacts(?:\/[^/]+)?)\/?$/,
    /^\/p\/[^/]+\/(?:tests|repos)(?:\/[^/]+)?\/?$/,
    /^\/p\/[^/]+\/logs\/(?:mail|events)\/?$/,
    /^\/p\/[^/]+\/settings\/(?:general|secrets|templates)\/?$/,
    /^\/p\/[^/]+\/knowledge\/(?:wiki(?:\/.*)?|memory|db(?:\/[^/]+)?)\/?$/,
  ].some((pattern) => pattern.test(path));
}

/** Transform only an inert sanitized fragment, before any nodes reach the live DOM. */
export function scopeRemoteHtml(el: DocumentFragment, instance: Pick<Instance, "href" | "path"> & Partial<Pick<Instance, "key" | "remoteOrigin">>, pageUrl = window.location.href) {
  const page = new URL(pageUrl);
  el.querySelectorAll<HTMLAnchorElement>("a").forEach((a) => {
    const block = () => {
      a.removeAttribute("href"); a.removeAttribute("target"); a.removeAttribute("rel");
      a.setAttribute("title", "Blocked remote content link");
    };
    const raw = a.getAttribute("href");
    if (raw === null) { block(); return; }
    try {
      const target = new URL(raw, page);
      if (!/^https?:$/.test(target.protocol)) { block(); return; }
      if (target.origin !== page.origin) {
        a.setAttribute("href", target.href);
        a.setAttribute("target", "_blank"); a.setAttribute("rel", "noopener noreferrer");
        return;
      }
      // Reject encoded separators/second decoding. Check decoded route names too,
      // so /%61pi and encoded traversal cannot become a home API request.
      if (/%(?:2f|5c|25|00)/i.test(target.pathname)) { block(); return; }
      const path = instance.path(target.pathname);
      const decoded = decodeURIComponent(path);
      if (decoded.includes("\\") || !dashboardPath(decoded)) { block(); return; }
      a.setAttribute("href", instance.href(path + target.search + target.hash));
      a.removeAttribute("target"); a.removeAttribute("rel");
    } catch { block(); }
  });
  el.querySelectorAll("img").forEach((img) => {
    const raw = remoteImageSource(img);
    const src = raw && instance.key ? remoteImagePath(raw, page, { ...instance, key: instance.key }) : undefined;
    if (src) { img.setAttribute("src", src); img.setAttribute("referrerpolicy", "no-referrer"); return; }
    const placeholder = document.createElement("span");
    placeholder.className = "small muted";
    placeholder.textContent = `[Image blocked${img.getAttribute("alt") ? `: ${img.getAttribute("alt")}` : ""}]`;
    img.replaceWith(placeholder);
  });
}
