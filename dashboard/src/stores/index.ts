// App-wide store instances. This is the only place that holds global state.
// Feature code imports from here; tests build their own with the factories.

import { createApi } from "../api";
import { createAgentsStore } from "./agents";
import { createAuthStore, signIn, signOut, upgradeToSession } from "./auth";
import { createEventsStore } from "./events";
import { createLeasesStore } from "./leases";
import { createProjectsStore } from "./projects";
import { createQuotaStore } from "./quota";
import { createSubscriptionsStore } from "./subscriptions";
import { createThemeStore } from "./theme";
import { createUnreadStore } from "./unread";

const storage = (() => {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
})();

const session = (() => {
  try {
    return typeof sessionStorage === "undefined" ? null : sessionStorage;
  } catch {
    return null;
  }
})();

export const auth = createAuthStore({ session, local: storage });
export const api = createApi(auth);
export const events = createEventsStore(api, { ready: () => auth.mode() !== "none" });
export const projects = createProjectsStore(api, storage);
export const agents = createAgentsStore(api, events);
export const subscriptions = createSubscriptionsStore(api, events);
export const quota = createQuotaStore(events);
export const unread = createUnreadStore(api, events);
export const leases = createLeasesStore(api, events);

// A token from an older build, or from a daemon that had no sessions:
// trade it for a cookie session now, so that it leaves the browser.
void upgradeToSession(api, auth);

/** Sign in with the token from the token screen. After an expiry, the event socket reconnects at once. */
export async function signInWithToken(token: string, remember: boolean) {
  const result = await signIn(api, auth, token, remember);
  events.reconnect();
  return result;
}

/** Sign out: close the event socket, `DELETE /api/session`, and forget every local copy. */
export async function signOutNow() {
  events.stop();
  await signOut(api, auth);
}

export { createBoardStore } from "./board";
export const theme = createThemeStore();
