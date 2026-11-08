import type { BoardGroup, Card } from "../../api/types";

export interface GroupSection {
  group?: BoardGroup;
  cards: Card[];
}

/** Keep members in their real column and group them in membership order. */
export function groupSections(cards: readonly Card[], column: string): GroupSection[] {
  const sections: GroupSection[] = [];
  const groups = new Map<string, GroupSection>();
  for (const card of cards.filter((c) => c.column === column)) {
    const group = card.group?.closed_at ? undefined : card.group;
    // A populated group's home card is metadata, not a member of this
    // column. Only ordinary members can keep its header here.
    if (card.kind === "group" && group && group.members.length > 0) continue;
    if (!group) {
      sections.push({ cards: [card] });
      continue;
    }
    let section = groups.get(group.id);
    if (!section) {
      section = { group, cards: [] };
      groups.set(group.id, section);
      sections.push(section);
    }
    if (card.kind !== "group") section.cards.push(card);
  }
  for (const section of sections) {
    const group = section.group;
    if (group) section.cards.sort((a, b) => group.members.indexOf(a.id) - group.members.indexOf(b.id));
  }
  return sections;
}
