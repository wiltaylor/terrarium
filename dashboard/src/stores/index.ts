// App-wide store instances. This is the only place that holds global state.
// Feature code imports from here; tests build their own with the factories.

import { createApi } from "../api";
import { createAgentsStore } from "./agents";
import { createBookmarksStore } from "./bookmarks";
import { changePassword, createAuthStore, restoreSession, signIn, signInWithPassword, signOut, signOutEverywhere } from "./auth";
import { createEventsStore } from "./events";
import { createInboxStore } from "./inbox";
import { createLeasesStore } from "./leases";
import { createPinsStore } from "./pins";
import { createProjectFoldersStore } from "./projectFolders";
import { createProjectsStore } from "./projects";
import { createQuotaStore } from "./quota";
import { createReposStore } from "./repos";
import { createSubscriptionsStore } from "./subscriptions";
import { createThemeStore } from "./theme";

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
export const bookmarks = createBookmarksStore(storage, window.location.origin, api.bookmarks);
export const events = createEventsStore(api, { ready: () => auth.mode() === "session" || auth.mode() === "bearer" });
export const projects = createProjectsStore(api, storage);
export const projectFolders = createProjectFoldersStore(api);
export const agents = createAgentsStore(api, events);
export const repos = createReposStore(api);
export const subscriptions = createSubscriptionsStore(api, events);
export const quota = createQuotaStore(events);
export const leases = createLeasesStore(api, events);
export const inbox = createInboxStore(api, events);
export const pins = createPinsStore(api, events, storage);

// At load: the cookie is the cache. `GET /api/session` says whether it is a
// session, and gives the CSRF value. A token from an older build (bearer
// mode) is traded for a cookie session, so that it leaves the browser.
void restoreSession(api, auth).then((ok) => {
  if (ok) events.start();
});

/** Sign in with the token ("Use the token instead"). After an expiry, the event socket reconnects at once. */
export async function signInWithToken(token: string) {
  const result = await signIn(api, auth, token);
  events.start();
  events.reconnect();
  return result;
}

/** Sign in with the operator password. */
export async function signInPassword(password: string) {
  await signInWithPassword(api, auth, password);
  events.start();
  events.reconnect();
}

/** Sign out: close the event socket, `DELETE /api/session`, and forget every local copy. */
export async function signOutNow() {
  events.stop();
  await signOut(api, auth);
}

/** Log out everywhere: `DELETE /api/sessions` ends every browser and CLI session. */
export async function signOutAll() {
  events.stop();
  await signOutEverywhere(api, auth);
}

/** Change the operator password. A cookie session signs in again with the new one. */
export async function changePasswordNow(current: string, next: string) {
  await changePassword(api, auth, current, next);
  events.start();
  events.reconnect();
}

export { createBoardStore } from "./board";
export const theme = createThemeStore();
