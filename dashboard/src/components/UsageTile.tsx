import { For, Show } from "solid-js";
import type { LoginStatus, Subscription } from "../api/types";
import { ago, compact, humanize, sparkPath, timeUntil, usageLevel, money, windowValue } from "../lib/format";
import { ProviderLogo } from "./ui";

export function Sparkline(props: { values: number[]; label?: string; height?: number }) {
  const w = 200;
  const h = () => props.height ?? 28;
  return (
    <svg class="sparkline" viewBox={`0 0 ${w} ${h()}`} style={{ height: `${h()}px` }} preserveAspectRatio="none" role="img" aria-label={props.label ?? "Usage over the last 24 hours"}>
      <Show when={props.values.some((v) => v > 0)} fallback={<line x1="0" y1={h() - 1} x2={w} y2={h() - 1} stroke="currentColor" stroke-dasharray="3 3" opacity="0.4" />}>
        <path d={sparkPath(props.values, w, h())} fill="none" stroke="currentColor" stroke-width="1.6" vector-effect="non-scaling-stroke" />
      </Show>
    </svg>
  );
}

export const LOGIN_CHIP: Record<LoginStatus, string> = { logged_in: "ok", expiring: "warn", expired: "bad", missing: "bad", unknown: "", api_key: "ok" };

/** No probe, or the probe has not run: local accounting is the only number. */
export const isLocalOnly = (s: Pick<Subscription, "local_estimate_only" | "probe_status">): boolean => s.local_estimate_only || s.probe_status === "none";

export function UsageTile(props: { sub: Subscription; now: number; spark: number[]; tokens24h?: number; onClick?: () => void }) {
  const local = () => isLocalOnly(props.sub);
  return (
    <div
      class={`card-box usage-tile${local() ? " local" : ""}`}
      onClick={() => props.onClick?.()}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && props.onClick?.()}
      role={props.onClick ? "button" : undefined}
      tabIndex={props.onClick ? 0 : undefined}
      style={{ cursor: props.onClick ? "pointer" : undefined }}
    >
      <div class="spread" style={{ "align-items": "flex-start" }}>
        <div class="provider">
          <ProviderLogo provider={props.sub.provider} />
          <div style={{ "min-width": 0 }}>
            <div style={{ "font-weight": 600 }}>{props.sub.plan ?? props.sub.name}</div>
            <div class="muted small">{props.sub.name}</div>
          </div>
        </div>
        <div class="row" style={{ gap: "4px", "flex-wrap": "wrap", "justify-content": "flex-end" }}>
          <Show when={props.sub.login_status !== "logged_in"}>
            <span class={`chip ${LOGIN_CHIP[props.sub.login_status]}`}>{humanize(props.sub.login_status)}</span>
          </Show>
          <Show when={props.sub.probe_status === "failed"}>
            <span class="chip warn" title={props.sub.probe_error}>
              probe failed
            </span>
          </Show>
          <Show when={props.sub.held_agents > 0}>
            <span class="chip bad" title="Agents whose turns wait after a usage-limit error">
              {props.sub.held_agents} held
            </span>
          </Show>
        </div>
      </div>
      <Show when={local()}>
        <div class="muted small">{props.sub.local_estimate_only ? "Local estimate only. This plan has no usage probe." : "Local estimate only. The probe has not run yet."}</div>
      </Show>
      <Show when={props.sub.windows.length} fallback={<Show when={!local()}><div class="muted small">No samples yet.</div></Show>}>
        <For each={props.sub.windows}>
          {(w) => {
            const level = () => usageLevel(w.used_pct, props.sub.alert_at);
            return (
              <div class="meter">
                <div class="meter-label">
                  <span>
                    <strong style={{ color: "var(--text)" }}>{w.window}</strong> {windowValue(w)}
                  </span>
                  <span>{w.balance ? `${money(w.balance.used, w.balance.currency)} used of ${money(w.balance.total, w.balance.currency)}` : w.resets_at ? `resets in ${timeUntil(w.resets_at, props.now)}` : "no reset time"}</span>
                </div>
                <div class={`meter-bar ${level()}`} role="progressbar" aria-valuenow={w.used_pct} aria-valuemin={0} aria-valuemax={100} aria-label={`${w.window} window`}>
                  <span style={{ width: `${Math.min(100, w.used_pct)}%` }} />
                </div>
              </div>
            );
          }}
        </For>
        <Show when={props.sub.probe_status === "failed" && props.sub.last_success_at}>
          <div class="muted small">As of the last good probe, {ago(props.sub.last_success_at, props.now)}.</div>
        </Show>
      </Show>
      <div class="spread small muted">
        <span>Tokens, 24 h (local)</span>
        <span class="mono">{compact(props.tokens24h ?? props.spark.reduce((a, b) => a + b, 0))}</span>
      </div>
      <Sparkline values={props.spark} label={`Tokens per hour on ${props.sub.name} over the last 24 hours`} />
    </div>
  );
}
