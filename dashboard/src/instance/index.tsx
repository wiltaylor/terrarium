import { createContext, useContext, type ParentProps } from "solid-js";
import { auth, changePasswordNow, localInstance, signInPassword, signInWithToken, signOutAll, signOutNow, theme, createBoardStore } from "../stores";
import type { Instance } from "./create";

const InstanceContext = createContext<Instance>();

/** Mount a separate keyed subtree per instance. Its creator owns disposal. */
export function InstanceProvider(props: ParentProps<{ instance: Instance }>) {
  return <InstanceContext.Provider value={props.instance}>{props.children}</InstanceContext.Provider>;
}

export function useInstance() {
  const instance = useContext(InstanceContext) ?? localInstance;
  // Authentication and appearance belong to the home browser, not the remote.
  return { ...instance, auth, theme, signInPassword, signInWithToken, signOutNow, signOutAll, changePasswordNow, createBoardStore };
}

export { createInstance, instanceHref, type Instance, type InstanceKey } from "./create";
