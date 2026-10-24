import type { Message } from "../../api/types";

export interface Thread {
  id: string;
  subject: string;
  participants: string[];
  messages: Message[];
  last_at: string;
  unread: number;
}

/** Group messages into threads, newest thread first, messages oldest first. */
export function groupThreads(messages: Message[]): Thread[] {
  const map = new Map<string, Message[]>();
  for (const m of messages) map.set(m.thread_id, [...(map.get(m.thread_id) ?? []), m]);
  return [...map.entries()]
    .map(([id, msgs]) => {
      const sorted = msgs.slice().sort((a, b) => a.created_at.localeCompare(b.created_at));
      return {
        id,
        subject: (sorted.find((m) => m.subject)?.subject ?? "").replace(/^Re:\s*/i, ""),
        participants: [...new Set(sorted.flatMap((m) => [m.from_name, m.to_name]))],
        messages: sorted,
        last_at: sorted.at(-1)!.created_at,
        unread: sorted.filter((m) => m.state === "queued" || m.state === "delivered").length,
      };
    })
    .sort((a, b) => b.last_at.localeCompare(a.last_at));
}
