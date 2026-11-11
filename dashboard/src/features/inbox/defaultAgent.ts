import { useInstance } from "../../instance";
import { createResource } from "solid-js";


/** A preference lookup must not prevent composing when an older daemon lacks it. */
export function useDefaultAgent() {
  const { api } = useInstance();
  const [preference] = createResource(async () => {
    try {
      return (await api.account.preferences()).default_agent ?? "";
    } catch {
      return "";
    }
  });
  return preference;
}
