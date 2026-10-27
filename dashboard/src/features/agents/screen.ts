// The Screen panel of the agent page: pure helpers, tested in screen.test.ts.
//
// The panel shows `GET /api/agents/{id}/screen` (docs/api.md §6.5) as text
// only. The daemon sends no escape codes; `screenText` removes any that
// come all the same, and the panel renders the result as a text node in a
// <pre>, never as HTML.

import type { AgentScreen } from "../../api/types";

/** The auto-refresh interval while the panel is open and the page is visible. */
export const SCREEN_REFRESH_MS = 5000;

// CSI, OSC (BEL or ST end), and other two-byte escapes; then C0 controls but tab and newline.
// eslint-disable-next-line no-control-regex
const ESCAPES = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-_]/g;
// eslint-disable-next-line no-control-regex
const CONTROLS = /[\x00-\x08\x0b-\x1f\x7f]/g;

/** The screen as plain text: no escape codes, no control characters, no trailing blanks. */
export function screenText(s: Pick<AgentScreen, "text">): string {
  return s.text
    .replace(ESCAPES, "")
    .replace(CONTROLS, "")
    .split("\n")
    .map((l) => l.trimEnd())
    .join("\n");
}

/** The frame line above the screen: `p/w · working · 24×80`. */
export function screenTitle(s: AgentScreen): string {
  return `${s.agent} · ${s.status} · ${s.rows}×${s.cols}`;
}

/** Refresh by itself only while the panel is open, the page is visible, and the agent runs. */
export function shouldAutoRefresh(o: { auto: boolean; pageVisible: boolean; running: boolean }): boolean {
  return o.auto && o.pageVisible && o.running;
}
