// A footer that says which calls of a view are not settled in docs/api.md:
// new in the daemon, or mock-only. In mock mode the mock serves them; a
// daemon without them answers "Not available yet".

import type { JSX } from "solid-js";
import { isMock } from "../api";

export function MockOnlyNote(props: { label?: string; children: JSX.Element }) {
  return (
    <footer class="mock-only-foot muted small" aria-label="API status of this page">
      <span class={`chip ${isMock ? "info" : "warn"}`}>{props.label ?? "mock-only"}</span> {props.children}{" "}
      {isMock ? "This page runs on the mock now." : "A daemon without these calls answers “Not available yet”."}
    </footer>
  );
}
