import type { Link } from "../../api/types";

/** The status chip class of a link. */
export function linkChip(l: Pick<Link, "status">): string {
  return l.status === "active" ? "ok" : l.status === "revoked" ? "bad" : "warn";
}
