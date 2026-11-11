export type HomeColumn = "in_progress" | "review" | "accepted" | "blocked";
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
  let cursor: string | undefined;
  do {
    const result = await page(cursor);
    items.push(...result.items);
    cursor = result.next ?? undefined;
  } while (cursor !== undefined);
  return items;
}
