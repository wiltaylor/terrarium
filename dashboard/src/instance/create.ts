import { createRoot, createSignal } from "solid-js";
import { createHttpApi, type Api } from "../api/client";
import { createApi } from "../api";
import { createHttpTransport } from "../api/http";
import type { AuthStore } from "../stores/auth";
import { createExpandedStore } from "../app/treeModel";
import { browserStorage, scopeStorage, type InstanceStorage } from "./storage";
import { createAgentsStore } from "../stores/agents";
import { createBookmarksStore } from "../stores/bookmarks";
import { createCardPeekStore } from "../stores/cardPeek";
import { createEventsStore } from "../stores/events";
import { createInboxStore } from "../stores/inbox";
import { createLeasesStore } from "../stores/leases";
import { createPinsStore } from "../stores/pins";
import { createProjectFoldersStore } from "../stores/projectFolders";
import { createProjectsStore } from "../stores/projects";
import { createQuotaStore } from "../stores/quota";
import { createReposStore } from "../stores/repos";
import { createSubscriptionsStore } from "../stores/subscriptions";

export type InstanceKey = "local" | `remote:${string}`;

/** Only same-origin dashboard paths are rebased. External and fragment links stay unchanged. */
export function instanceHref(name: string | undefined, path: string): string {
  if (!name || !path.startsWith("/") || path.startsWith("//")) return path;
  // Normalize before prefixing, so dot segments cannot leave the instance subtree.
  const target = new URL(path, "https://dashboard.invalid");
  if (target.origin !== "https://dashboard.invalid") return path;
  path = target.pathname + target.search + target.hash;
  const prefix = `/remote/${name}`;
  if (path === prefix || path.startsWith(prefix + "/") || path.startsWith(prefix + "?") || path.startsWith(prefix + "#")) return path;
  return prefix + path;
}

export function createInstance(options: {
  name?: string;
  auth: AuthStore;
  api?: Api;
  storage?: InstanceStorage | null;
  session?: InstanceStorage | null;
}) {
  const { name, auth } = options;
  if (name !== undefined && !/^[a-z][a-z0-9-]{0,31}$/.test(name)) throw new Error("Invalid remote name");
  const key: InstanceKey = name === undefined ? "local" : `remote:${name}`;
  const storage = scopeStorage(options.storage === undefined ? browserStorage("localStorage") : options.storage, key);
  const session = scopeStorage(options.session === undefined ? browserStorage("sessionStorage") : options.session, key);
  // These are HOME credentials, never a remote token. A remote 401 must not sign out the home.
  const api = options.api ?? (name === undefined ? createApi(auth) : createHttpApi(createHttpTransport({
    baseUrl: `/api/remotes/${name}/proxy`,
    getToken: auth.token,
    getCsrf: auth.csrf,
  })));
  return createRoot((dispose) => {
    const bookmarks = createBookmarksStore(storage, window.location.origin, api.bookmarks);
    const events = createEventsStore(api, { ready: () => auth.mode() === "session" || auth.mode() === "bearer" });
    const projects = createProjectsStore(api, storage);
    const projectFolders = createProjectFoldersStore(api);
    const agents = createAgentsStore(api, events);
    const repos = createReposStore(api);
    const subscriptions = createSubscriptionsStore(api, events);
    const quota = createQuotaStore(events);
    const leases = createLeasesStore(api, events);
    const inbox = createInboxStore(api, events);
    const pins = createPinsStore(api, events, storage);
    const cardPeek = createCardPeekStore();

    const expanded = createExpandedStore(storage);
    const [promptVersion, setPromptVersion] = createSignal(0);
    const promptLibrary = { version: promptVersion, refresh: () => setPromptVersion((v) => v + 1) };
    return {
      key, api, storage, session, expanded, promptLibrary,
      apiHref: (path: string) => `${name === undefined ? import.meta.env.VITE_API_BASE ?? "" : `/api/remotes/${name}/proxy`}${path}`,
      href: (path: string) => instanceHref(name, path),
      path: (path: string) => name === undefined ? path : path.replace(new RegExp(`^/remote/${name}(?=/|$)`), "") || "/",
      bookmarks, events, projects, projectFolders, agents, repos, subscriptions, quota, leases, inbox, pins, cardPeek,
      dispose: () => {
        events.stop();
        subscriptions.stop();
        agents.dispose();
        inbox.dispose();
        pins.dispose();
        leases.dispose();
        quota.dispose();
        dispose();
      },
    };
  });
}

export type Instance = ReturnType<typeof createInstance>;
