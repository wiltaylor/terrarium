// Theme preference: system, light, or dark. Stamped as data-theme on <html>.

import { createSignal } from "solid-js";

export type ThemePref = "system" | "light" | "dark";
const KEY = "terrarium.theme";

export function createThemeStore(root: HTMLElement | null = typeof document === "undefined" ? null : document.documentElement) {
  let initial: ThemePref = "system";
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark") initial = v;
  } catch {
    // ignore
  }
  const [pref, setPrefSignal] = createSignal<ThemePref>(initial);
  const apply = (p: ThemePref) => {
    if (!root) return;
    if (p === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", p);
  };
  apply(initial);
  const set = (p: ThemePref) => {
    setPrefSignal(p);
    apply(p);
    try {
      localStorage.setItem(KEY, p);
    } catch {
      // ignore
    }
  };
  return {
    pref,
    set,
    cycle: () => set(pref() === "system" ? "light" : pref() === "light" ? "dark" : "system"),
  };
}
