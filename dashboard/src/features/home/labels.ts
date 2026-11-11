import { useInstance } from "../../instance";
import type { UsageGroup } from "../../api/types";


/** A usage key as a name: an agent ULID (the `agent` group key) becomes `project/name`. */
export const usageLabel = (group: UsageGroup) => {
  const { agents } = useInstance();
  return (key: string) => {
  if (group !== "agent") return key;
  const a = agents.byId(key);
  return a ? `${a.project}/${a.name}` : key;
};
};
