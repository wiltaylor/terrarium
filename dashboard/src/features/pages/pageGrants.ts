// The grants of the page widget parts (TER-1077), from the caller's rights
// in the page list (`GET /api/projects/{name}/pages`, `rights`). Each grant
// is one existing server policy check, never the layout `can_edit`. A missing
// or failed rights read gives no grant: the widgets are read-only. The
// server still checks every write.

import type { EventSource, ProjectPageRights, Schedule } from "../../api/types";
import type { BoardTestsAgentsGrants } from "./parts/boardTestsAgents";
import type { OperationsGrants } from "./parts/operations";

export interface PageGrants { boardTestsAgents: BoardTestsAgentsGrants; operations: OperationsGrants }

const yes = (value: unknown) => value === true;

export function pageGrants(rights: ProjectPageRights | undefined, project: string): PageGrants {
  const r = rights ?? undefined;
  const admin = yes(r?.admin_project);
  const global = yes(r?.manage_global);
  const schedules = yes(r?.manage_schedules);
  // A schedule or event source of this project needs the project right, a global one the global right.
  const own = (item: { project?: string | null }) => (item.project ? item.project === project : false);
  return {
    boardTestsAgents: {
      canWriteCards: yes(r?.write_project),
      canEditColumns: admin,
      canCreateAgent: admin,
      canPinAgents: yes(r?.operator),
    },
    operations: {
      createSchedule: schedules,
      manageSchedule: (s: Schedule) => (s.project ? own(s) && schedules : global),
      createEventSource: admin,
      manageEventSource: (s: EventSource) => (s.project ? own(s) && admin : global),
      manageRules: admin,
      managePipelines: admin,
    },
  };
}
