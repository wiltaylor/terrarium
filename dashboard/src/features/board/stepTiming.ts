import type { BoardGroup } from "../../api/types";
import { span } from "../../lib/eta";

export function stepTiming(step: BoardGroup["steps"][number], now: number): string {
  if (step.duration_secs != null) return `took ${span(step.duration_secs * 1000)}`;
  if (step.state === "done") return "done";
  if (step.state === "failed") return "failed";
  if (step.eta_at) {
    const left = Date.parse(step.eta_at) - now;
    if (Number.isFinite(left)) return left < 0 ? `overdue ${span(-left)}` : `${span(left)} left`;
  }
  return "ETA unavailable (no history)";
}
