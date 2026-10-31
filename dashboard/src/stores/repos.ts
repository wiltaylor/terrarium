// The repos of each project, for the Repos node of the sidebar tree. The
// list of a project loads the first time something asks for it. An add
// (AddRepoForm) puts the new repo into the list at once.

import { createStore, produce } from "solid-js/store";
import type { Api } from "../api/client";
import type { Repo } from "../api/types";

const byName = (a: Repo, b: Repo) => a.name.localeCompare(b.name);

export function createReposStore(api: { projects: Pick<Api["projects"], "repos"> }) {
  const [state, setState] = createStore<{ byProject: Record<string, Repo[]>; errors: Record<string, unknown> }>({ byProject: {}, errors: {} });
  const asked = new Set<string>();

  /** Read the repos of `project` from the daemon again. */
  const refresh = async (project: string) => {
    asked.add(project);
    try {
      const list = await api.projects.repos(project);
      setState("byProject", project, [...list].sort(byName));
      setState("errors", project, undefined);
    } catch (e) {
      setState("errors", project, e);
    }
  };

  return {
    state,
    refresh,
    /** Load the repos of `project` once. */
    load: (project: string) => {
      if (!asked.has(project)) void refresh(project);
    },
    /** The repos of `project` by name, or an empty list while they are not loaded. */
    inProject: (project: string): Repo[] => state.byProject[project] ?? [],
    /** True when the list of `project` is in. */
    loaded: (project: string) => project in state.byProject,
    /** Put a repo that was just added into the list of its project. */
    added(repo: Repo) {
      setState(
        produce((s) => {
          const list = (s.byProject[repo.project] ?? []).filter((r) => r.name !== repo.name);
          s.byProject[repo.project] = [...list, repo].sort(byName);
        }),
      );
    },
  };
}

export type ReposStore = ReturnType<typeof createReposStore>;
