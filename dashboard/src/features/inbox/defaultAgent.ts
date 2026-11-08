import { createResource } from "solid-js";
import { api } from "../../stores";

/** A preference lookup must not prevent composing when an older daemon lacks it. */
export function useDefaultAgent() {
  const [preference] = createResource(async () => {
    try {
      return (await api.account.preferences()).default_agent ?? "";
    } catch {
      return "";
    }
  });
  return preference;
}
