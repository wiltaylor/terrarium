export type HomeColumn = "in_progress" | "review" | "accepted" | "blocked";
export type HomeBoardColumns = Record<HomeColumn, string[]>;
export const defaultHomeColumns: HomeBoardColumns = {
  in_progress: ["working", "research"], review: ["review", "merge_queue"], accepted: ["accepted"], blocked: ["blocked"],
};
export interface HomeCard {
  id: string;
  title: string;
  project: string;
  labels: string[];
  column: string;
  home_column: HomeColumn;
  assignee: string | null;
  assignee_name: string | null;
  assignee_status: string | null;
  eta_at: string | null;
  blocked_reason: string | null;
}
export interface HomeAttention {
  id: string;
  project: string;
  name: string;
  status: string;
  link: string;
}
export interface HomePage<T> { items: T[]; next: string | null }
export interface HomeQuery { project?: string; cursor?: string; limit?: number }
export interface HomeApi {
  cards(q: HomeQuery & { column: HomeColumn }): Promise<HomePage<HomeCard>>;
  monitors(q?: HomeQuery): Promise<HomePage<HomeAttention>>;
  pipelines(q?: HomeQuery): Promise<HomePage<HomeAttention>>;
}

/** Empty pages still carry a continuation when the daemon filtered candidates. */
export async function allHome<T>(page: (cursor?: string) => Promise<HomePage<T>>): Promise<T[]> {
  const items: T[] = [];
  const seen = new Set<string>();
  let pages = 0;
  let cursor: string | undefined;
  do {
    const result = await page(cursor);
    pages++;
    items.push(...result.items);
    cursor = result.next ?? undefined;
    if (cursor !== undefined) {
      if (seen.has(cursor)) throw new Error("Home pagination returned a repeated cursor.");
      if (pages >= 500) throw new Error("Home pagination exceeded the 500-page limit.");
      seen.add(cursor);
    }
  } while (cursor !== undefined);
  return items;
}
