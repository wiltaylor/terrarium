import { createExpandedStore } from "../app/treeModel";

/** Existing local component fixtures expose the same stores through the instance boundary. */
export function instanceTestStores<T extends object>(stores: T) {
  return {
    auth: undefined, theme: undefined, signInPassword: undefined, signInWithToken: undefined,
    signOutNow: undefined, signOutAll: undefined, changePasswordNow: undefined, createBoardStore: undefined,
    ...stores,
    localInstance: {
      key: "local", href: (p: string) => p, path: (p: string) => p,
      storage: window.localStorage, session: window.sessionStorage,
      expanded: createExpandedStore(window.localStorage),
      ...stores,
    },
  };
}
