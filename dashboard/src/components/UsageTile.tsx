import { For, Show } from "solid-js";
import type { Subscription } from "../api/types";
import { humanize, sparkPath, timeUntil, usageLevel } from "../lib/format";
import { ProviderLogo } from "./ui";

export function Sparkline(props: { values: number[] }) {
  const w = 200;
  const h = 28;
  return (
    <svg class="sparkline" viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" role="img" aria-label="Usage over the last 24 hours">
      <Show when={props.values.length > 1} fallback={<line x1="0" y1={h - 1} x2={w} y2={h - 1} stroke="currentColor" stroke-dasharray="3 3" opacity="0.4" />}>
        <path d={sparkPath(props.values, w, h)} fill="none" stroke="currentColor" stroke-width="1.6" vector-effect="non-scaling-stroke" />
      </Show>
    </svg>
  );
}

export function UsageTile(props: { sub: Subscription; now: number; onClick?: () => void }) {
  const local = () => !props.sub.probe;
  return (
    <div class={`card-box usage-tile${local() ? " local" : ""}`} onClick={() => props.onClick?.()} style={{ cursor: props.onClick ? "pointer" : undefined }}>
      <div class="spread">
        <div class="provider">
          <ProviderLogo provider={props.sub.provider} />
          <div>
            <div style={{ "font-weight": 600 }}>{props.sub.plan}</div>
            <div class="muted small">{props.sub.name}</div>
          </div>
        </div>
        <Show when={props.sub.status !== "logged_in"}>
          <span class={`chip ${props.sub.status === "expiring" ? "warn" : "bad"}`}>{humanize(props.sub.status)}</span>
        </Show>
      </div>
      <Show when={!local()} fallback={<div class="muted small">Local estimate only. This plan has no usage probe.</div>}>
        <Show when={props.sub.windows.length} fallback={<div class="muted small">No samples yet.</div>}>
          <For each={props.sub.windows}>
            {(w) => {
              const level = () => usageLevel(w.used_pct, props.sub.alert_at);
              return (
                <div class="meter">
                  <div class="meter-label">
                    <span>
                      <strong style={{ color: "var(--text)" }}>{w.window}</strong> {w.used_pct}%
                    </span>
                    <span>resets in {timeUntil(w.resets_at, props.now)}</span>
                  </div>
                  <div class={`meter-bar ${level()}`} role="progressbar" aria-valuenow={w.used_pct} aria-valuemin={0} aria-valuemax={100} aria-label={`${w.window} window`}>
                    <span style={{ width: `${Math.min(100, w.used_pct)}%` }} />
                  </div>
                </div>
              );
            }}
          </For>
        </Show>
      </Show>
      <Sparkline values={props.sub.history} />
    </div>
  );
}
