// App-wide store instances. This is the only place that holds global state.
// Feature code imports from here; tests build their own with the factories.

import { createApi } from "../api";
import { createAgentsStore } from "./agents";
import { createAuthStore } from "./auth";
import { createEventsStore } from "./events";
import { createProjectsStore } from "./projects";
import { createSubscriptionsStore } from "./subscriptions";
import { createThemeStore } from "./theme";

const storage = (() => {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
})();

export const auth = createAuthStore(storage);
export const api = createApi(auth);
export const events = createEventsStore(api);
export const projects = createProjectsStore(api, storage);
export const agents = createAgentsStore(api, events);
export const subscriptions = createSubscriptionsStore(api);

export { createBoardStore } from "./board";
export const theme = createThemeStore();
