// The grants of the page widget parts (TER-1077), from the caller's rights
// in the page list (`GET /api/projects/{name}/pages`, `rights`). Each grant
// is one existing server policy check, never the layout `can_edit`. A missing
// or failed rights read gives no grant: the widgets are read-only. The
// server still checks every write.

import type { EventSource, ProjectPageRights, Schedule } from "../../api/types";
import type { BoardTestsAgentsGrants } from "./parts/boardTestsAgents";
import type { KnowledgeLogsReposGrants } from "./parts/knowledgeLogsRepos";
import type { OperationsGrants } from "./parts/operations";

export interface PageGrants { boardTestsAgents: BoardTestsAgentsGrants; operations: OperationsGrants; knowledgeLogsRepos: KnowledgeLogsReposGrants }

const yes = (value: unknown) => value === true;

export function pageGrants(rights: ProjectPageRights | undefined, project: string): PageGrants {
  const r = rights ?? undefined;
  const admin = yes(r?.admin_project);
  const write = yes(r?.write_project);
  const global = yes(r?.manage_global);
  const schedules = yes(r?.manage_schedules);
  // A schedule or event source of this project needs the project right, a global one the global right.
  const own = (item: { project?: string | null }) => (item.project ? item.project === project : false);
  return {
    boardTestsAgents: {
      canWriteCards: write,
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
    // TER-1134: the same server checks as the Repos, Knowledge and Logs endpoints.
    knowledgeLogsRepos: {
      attachRepo: admin, // admin_project, "attaching a repo"
      checkoutRepo: write, // write_project, then the target check
      writeWiki: write, // write_project (create, save, review, restore)
      writeDb: write, // write_db, which is write_project
      purgeDbType: admin, // purge_db_type, a type with records
      cardFromMail: write, // write_project of the card create
      replyAsProject: admin, // read_inbox, the project inbox reply
    },
  };
}
