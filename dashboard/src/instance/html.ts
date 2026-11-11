/** Rewrite sanitized links before insertion, including modified-click/new-tab targets. */
export function scopeHtmlLinks(el: HTMLElement, href: (path: string) => string) {
  el.querySelectorAll<HTMLAnchorElement>("a[href]").forEach((a) => {
    const path = a.getAttribute("href") ?? "";
    if (path.startsWith("/") && !path.startsWith("//")) a.setAttribute("href", href(path));
  });
}
