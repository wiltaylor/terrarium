/** Browser preferences are never shared between daemons. Local keys stay compatible. */
export type InstanceStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export function browserStorage(kind: "localStorage" | "sessionStorage"): InstanceStorage | null {
  try {
    return typeof window === "undefined" ? null : window[kind];
  } catch {
    return null;
  }
}

export function scopeStorage(storage: InstanceStorage | null, key: string): InstanceStorage | null {
  if (!storage || key === "local") return storage;
  const prefix = `terrarium.instance.${key}:`;
  return {
    getItem: (k) => storage.getItem(prefix + k),
    setItem: (k, v) => storage.setItem(prefix + k, v),
    removeItem: (k) => storage.removeItem(prefix + k),
  };
}
