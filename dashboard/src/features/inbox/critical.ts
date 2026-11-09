// Critical notices and questions (docs/api.md §6.8, priority `critical`),
// also the ones inside an open thread (TER-488): the banner on every page and the flashing browser tab. Pure helpers and a
// small state machine with its timers and document injected. No Solid
// imports, so the tests drive them with fake timers.

import type { InboxItem } from "../../api/types";
import { isCritical } from "./model";
import { threadHref } from "./threads";

/** What the banner shows: the newest open critical item, and how many more there are. */
export interface CriticalBanner {
  item: InboxItem;
  /** The other open critical items. */
  more: number;
  /** The asking agent (`project/name`), else the project. */
  from: string;
  /** The short text of the item: its first line, at most `SHORT` characters. */
  text: string;
  /**
   * The Inbox link that scrolls to the item and highlights it. For a thread (TER-488): the thread at
   * `/threads/:id`, or the Threads screen when more than one thread has a critical question.
   */
  href: string;
}

const SHORT = 140;

/** The first line of a text, cut at `max` characters with an ellipsis. */
export function shortText(text: string, max = SHORT): string {
  const line = (text.split("\n").find((l) => l.trim()) ?? "").trim();
  return line.length > max ? `${line.slice(0, max - 1).trimEnd()}…` : line;
}

/** The Inbox link of an item: `/inbox?item=<id>`. */
export const inboxItemHref = (id: string): string => `/inbox?item=${encodeURIComponent(id)}`;

/** The DOM id of an item on the Inbox page. */
export const inboxItemDomId = (id: string): string => `inbox-item-${id.replace(/[^A-Za-z0-9_-]/g, "-")}`;

/** The asker of a thread item: its agent, else who opened it, as `project/name`; else the project. */
function threadFrom(item: InboxItem): string {
  const th = item.thread;
  const name = th?.agent_name ?? (th && th.opened_by !== "operator" ? th.opened_by_name : null);
  if (!name) return item.project ?? "";
  return name.includes("/") || !item.project ? name : `${item.project}/${name}`;
}

/**
 * The banner for the open items, or `null` when no open item is critical. The newest critical item shows.
 * A thread item (kind `thread`, TER-488) is critical while the thread has an open critical question; the
 * banner shows its topic, as a thread row has no question text.
 */
export function criticalBanner(open: InboxItem[]): CriticalBanner | null {
  const crit = open.filter(isCritical).sort((a, b) => b.at.localeCompare(a.at) || b.id.localeCompare(a.id));
  const item = crit[0];
  if (!item) return null;
  const more = crit.length - 1;
  if (item.kind === "thread" && item.thread) {
    const threads = crit.filter((i) => i.kind === "thread").length;
    return { item, more, from: threadFrom(item), text: shortText(item.thread.topic), href: threads > 1 ? "/threads" : threadHref(item.thread.id) };
  }
  const q = item.question;
  const from = q ? (q.agent_name.includes("/") ? q.agent_name : `${q.project}/${q.agent_name}`) : (item.project ?? "");
  const raw = q?.text ?? item.message?.subject ?? item.system?.subject ?? "";
  return { item, more, from, text: shortText(raw), href: inboxItemHref(item.id) };
}

/**
 * Announce the banner (`role="alert"`) only on the first appearance of an
 * item, not on each refresh of the list. `seen` holds the ids already
 * announced; the answer holds the next set.
 */
export function announce(seen: ReadonlySet<string>, id: string | null): { announce: boolean; seen: Set<string> } {
  if (!id || seen.has(id)) return { announce: false, seen: new Set(seen) };
  const next = new Set(seen);
  next.add(id);
  return { announce: true, seen: next };
}

// ----- the flashing tab -----

export const ALERT_TITLE = "⚠ Critical — Terrarium";
export const FLASH_MS = 1000;

/** A red triangle with a "!", as a data URI (the CSP allows `data:` images). */
export const ALERT_FAVICON =
  "data:image/svg+xml," +
  encodeURIComponent(
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><path d="M16 3 30 28H2Z" fill="#c53030"/><path d="M16 12v8" stroke="#fff" stroke-width="3" stroke-linecap="round"/><circle cx="16" cy="24" r="1.8" fill="#fff"/></svg>',
  );

/** What the flasher touches: the title, the favicon, and the timers. */
export interface FlashEnv {
  getTitle(): string;
  setTitle(title: string): void;
  /** The `href` of the favicon link, or `null` when the page has none. */
  getFavicon(): string | null;
  setFavicon(href: string): void;
  setInterval(fn: () => void, ms: number): unknown;
  clearInterval(handle: unknown): void;
}

export interface TitleFlasher {
  /**
   * Give the flasher the facts. `critical`: an open critical item exists.
   * `attentive`: the page is visible and has the focus. It flashes while
   * `critical && !attentive`, and stops (and restores the title and the
   * favicon) otherwise.
   */
  update(facts: { critical: boolean; attentive: boolean }): void;
  /** True while it flashes. */
  flashing(): boolean;
  /** Stop, and restore the title and the favicon. */
  dispose(): void;
}

export function createTitleFlasher(env: FlashEnv, periodMs = FLASH_MS): TitleFlasher {
  let timer: unknown = null;
  let base = "";
  let icon: string | null = null;
  let alertShown = false;

  const stop = () => {
    if (timer === null) return;
    env.clearInterval(timer);
    timer = null;
    env.setTitle(base);
    if (icon !== null) env.setFavicon(icon);
    alertShown = false;
  };

  const start = () => {
    if (timer !== null) return;
    base = env.getTitle();
    icon = env.getFavicon();
    if (icon !== null) env.setFavicon(ALERT_FAVICON);
    env.setTitle(ALERT_TITLE);
    alertShown = true;
    timer = env.setInterval(() => {
      // The page can change its title meanwhile: keep the newest one as the base.
      const now = env.getTitle();
      if (now !== ALERT_TITLE) base = now;
      alertShown = !alertShown;
      env.setTitle(alertShown ? ALERT_TITLE : base);
    }, periodMs);
  };

  return {
    update({ critical, attentive }) {
      if (critical && !attentive) start();
      else stop();
    },
    flashing: () => timer !== null,
    dispose: stop,
  };
}

/** The flasher's environment in a browser. */
export function browserFlashEnv(doc: Document = document, win: Window = window): FlashEnv {
  const link = () => doc.querySelector<HTMLLinkElement>('link[rel~="icon"]');
  return {
    getTitle: () => doc.title,
    setTitle: (t) => {
      doc.title = t;
    },
    getFavicon: () => link()?.getAttribute("href") ?? null,
    setFavicon: (href) => link()?.setAttribute("href", href),
    setInterval: (fn, ms) => win.setInterval(fn, ms),
    clearInterval: (h) => win.clearInterval(h as number),
  };
}
