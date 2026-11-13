// @vitest-environment jsdom
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { Board, Column } from "../../api/types";
import { ColumnsEditorFields } from "./ColumnsEditor";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); });
const columns: Column[] = [
  { name: "backlog", position: 0 }, { name: "working", position: 1, role: "working" }, { name: "done", position: 2 },
];
it("focuses fields, validates before save and closes only after successful save", async () => {
  const host = document.createElement("div"); document.body.append(host);
  const save = vi.fn(async () => ({ columns, cards: [], project: "p" }) as Board);
  const close = vi.fn();
  dispose = render(() => <ColumnsEditorFields columns={columns} onSave={save} onClose={close} />, host);
  expect(document.activeElement).toBe(host.querySelector('input:not([disabled])'));
  const name = host.querySelector<HTMLInputElement>('[data-column-row="working"] input')!;
  name.value = "";
  name.dispatchEvent(new Event("input", { bubbles: true }));
  host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  expect(save).not.toHaveBeenCalled();
  expect(host.querySelector('[aria-invalid="true"]')).not.toBeNull();
  name.value = "working";
  name.dispatchEvent(new Event("input", { bubbles: true }));
  host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await Promise.resolve();
  expect(save).toHaveBeenCalledTimes(1);
  expect(close).toHaveBeenCalledTimes(1);
});
it("cancels without saving", () => {
  const host = document.createElement("div"); document.body.append(host);
  const save = vi.fn();
  const close = vi.fn();
  dispose = render(() => <ColumnsEditorFields columns={columns} onSave={save} onClose={close} />, host);
  [...host.querySelectorAll("button")].find(b => b.textContent === "Cancel")!.click();
  expect(close).toHaveBeenCalledOnce();
  expect(save).not.toHaveBeenCalled();
});
