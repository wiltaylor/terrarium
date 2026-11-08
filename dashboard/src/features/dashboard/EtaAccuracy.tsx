import { Show } from "solid-js";
import type { DashboardView, EtaAccuracy as Accuracy } from "../../api/types";

function Metrics(props: { label: string; value: Accuracy }) {
  return <div>
    <h3>{props.label}</h3>
    <p>{props.value.samples} samples</p>
    <Show when={props.value.samples > 0} fallback={<p>No completed estimates with enough history yet.</p>}>
      <p>On time: {props.value.on_time_pct?.toFixed(0)}% · Within half to double: {props.value.within_half_to_double_pct?.toFixed(0)}%</p>
    </Show>
  </div>;
}

export function EtaAccuracy(props: { value: NonNullable<DashboardView["eta_accuracy"]> }) {
  return <section aria-label="ETA accuracy">
    <h2>ETA accuracy</h2>
    <p class="small muted">Latest 30 completed cards, each counted once. History is limited to the latest 2,000 project card moves.</p>
    <Metrics label="Raw worker guesses" value={props.value.raw} />
    <Metrics label="Calibrated backtest (p70 target: 70% on time)" value={props.value.calibrated} />
    <p class="small muted">Backtest uses only history completed before each guess. Cards without enough calibration history are excluded from calibrated samples. This is a historical simulation, not recorded displayed deadlines.</p>
  </section>;
}
