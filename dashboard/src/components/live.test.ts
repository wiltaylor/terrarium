import { describe, expect, it } from "vitest";
import { arrive, heldCount, LIVE_START, newKeys, release, setLive, shownOf } from "./live";

type Row = { id: string; project: string };
const row = (id: string, project = "p"): Row => ({ id, project });
const key = (r: Row) => r.id;

describe("live state", () => {
  it("shows new items at once while live", () => {
    expect(arrive(LIVE_START, ["A"])).toBe(LIVE_START);
    expect(shownOf([row("A"), row("B")], LIVE_START, key).map(key)).toEqual(["A", "B"]);
  });

  it("holds new items while off, once each", () => {
    let s = setLive(LIVE_START, false);
    s = arrive(s, ["A", "B"]);
    s = arrive(s, ["B", "C"]);
    expect(s).toEqual({ live: false, held: ["A", "B", "C"] });
    const items = [row("C"), row("B"), row("A"), row("X")];
    expect(shownOf(items, s, key).map(key)).toEqual(["X"]);
  });

  it("counts only the held items that pass the filters", () => {
    const s = arrive(setLive(LIVE_START, false), ["A", "B", "C"]);
    const filtered = [row("A", "p"), row("B", "q"), row("C", "p"), row("X", "p")].filter((r) => r.project === "p");
    expect(heldCount(filtered, s, key)).toBe(2);
    expect(heldCount(filtered, LIVE_START, key)).toBe(0);
  });

  it("shows the held items on release and stays off", () => {
    const s = release(arrive(setLive(LIVE_START, false), ["A"]));
    expect(s).toEqual({ live: false, held: [] });
    expect(arrive(s, ["B"]).held).toEqual(["B"]);
  });

  it("shows only the given held items, and keeps the others held", () => {
    const s = arrive(setLive(LIVE_START, false), ["A", "B"]);
    expect(release(s, ["B", "X"]).held).toEqual(["A"]);
    expect(release(s, ["X"])).toBe(s);
  });

  it("shows the held items when live goes on again", () => {
    const s = arrive(setLive(LIVE_START, false), ["A"]);
    expect(setLive(s, true)).toEqual(LIVE_START);
    expect(setLive(LIVE_START, true)).toBe(LIVE_START);
  });

  it("finds the new keys of a refresh", () => {
    expect(newKeys([row("A"), row("B")], [row("C"), row("A"), row("B"), row("D")], key)).toEqual(["C", "D"]);
  });
});
