import { createExpandedStore } from "../app/treeModel";
import { createSignal } from "solid-js";

/** Existing local component fixtures expose the same stores through the instance boundary. */
export function instanceTestStores<T extends object>(stores: T) {
  const [version, setVersion] = createSignal(0);
  return {
    auth: undefined, theme: undefined, signInPassword: undefined, signInWithToken: undefined,
    signOutNow: undefined, signOutAll: undefined, changePasswordNow: undefined, createBoardStore: undefined,
    ...stores,
    localInstance: {
      key: "local", href: (p: string) => p, path: (p: string) => p,
      storage: window.localStorage, session: window.sessionStorage,
      expanded: createExpandedStore(window.localStorage),
      promptLibrary: { version, refresh: () => setVersion((v) => v + 1) },
      ...stores,
    },
  };
}
