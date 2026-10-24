// Project list and the selected project. Every project view filters to it.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { Project } from "../api/types";
import type { KeyValueStorage } from "./auth";

export const PROJECT_KEY = "terrarium.project";

export function createProjectsStore(api: Pick<Api, "projects">, storage: KeyValueStorage | null = null) {
  const read = () => {
    try {
      return storage?.getItem(PROJECT_KEY) ?? null;
    } catch {
      return null;
    }
  };
  const [projects, setProjects] = createSignal<Project[]>([]);
  const [selected, setSelectedSignal] = createSignal<string | null>(read());

  const select = (name: string | null) => {
    setSelectedSignal(name);
    try {
      if (name) storage?.setItem(PROJECT_KEY, name);
      else storage?.removeItem(PROJECT_KEY);
    } catch {
      // ignore
    }
  };

  const refresh = async () => {
    const list = await api.projects.list();
    setProjects(list);
    const cur = selected();
    if (!cur || !list.some((p) => p.name === cur)) select(list[0]?.name ?? null);
    return list;
  };

  return {
    projects,
    selected,
    select,
    refresh,
    prefixes: () => Object.fromEntries(projects().map((p) => [p.card_prefix, p.name])),
  };
}

export type ProjectsStore = ReturnType<typeof createProjectsStore>;
