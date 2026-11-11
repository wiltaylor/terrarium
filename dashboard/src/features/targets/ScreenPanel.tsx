import { useInstance } from "../../instance";
// The live view of a target display (docs/api.md §17.10): a screenshot
// every 2.5 s while the panel shows and the page is visible, a pause
// button, the size, the backend, and the operations. The operator can turn
// on "Click to send": a click on the image sends a click at the same
// screen pixel. The daemon refuses input without the right
// (`409`, an exclusive lease) and the panel shows its message.

import { createResource, createSignal, onCleanup, onMount, Show } from "solid-js";
import { ApiError } from "../../api/http";
import type { DisplayShot, Target } from "../../api/types";
import { ErrorBanner } from "../../components/ui";

import { createRefresher, mapClick, shotSrc } from "./display";

export function ScreenPanel(props: { target: Target; onClose: () => void }) {
  const { api } = useInstance();
  const [shot, setShot] = createSignal<DisplayShot | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [paused, setPaused] = createSignal(false);
  const [clickToSend, setClickToSend] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [info] = createResource(
    () => props.target.name,
    (name) => api.targets.displayInfo(name).catch((e: unknown) => (setError(e), null)),
  );

  const refresher = createRefresher(async () => {
    try {
      setShot(await api.targets.screenshot(props.target.name));
      setError(null);
      return true;
    } catch (e) {
      setError(e);
      return false;
    }
  });

  onMount(() => {
    const visible = () => document.visibilityState !== "hidden";
    const onVis = () => refresher.setVisible(visible());
    document.addEventListener("visibilitychange", onVis);
    refresher.setVisible(visible());
    onCleanup(() => {
      document.removeEventListener("visibilitychange", onVis);
      refresher.dispose();
    });
  });

  const togglePause = () => {
    const p = !paused();
    setPaused(p);
    refresher.setPaused(p);
  };

  const onImageClick = async (e: MouseEvent) => {
    const s = shot();
    if (!clickToSend() || !s) return;
    const img = e.currentTarget as HTMLImageElement;
    const box = img.getBoundingClientRect();
    const at = mapClick({ x: e.clientX - box.left, y: e.clientY - box.top }, { width: box.width, height: box.height }, s);
    try {
      await api.targets.displayOp(props.target.name, "click", at);
      setNotice(`Clicked at ${at.x},${at.y}.`);
      refresher.now();
    } catch (err) {
      setNotice(err instanceof ApiError ? `Refused (${err.status}): ${err.message}` : String(err));
    }
  };

  return (
    <div class="stack" style={{ gap: "6px" }} aria-label={`Screen of ${props.target.name}`}>
      <div class="spread">
        <strong>Screen</strong>
        <span class="row" style={{ gap: "8px", "flex-wrap": "wrap" }}>
          <Show when={info()}>
            {(i) => (
              <span class="small muted" aria-label="Display info">
                {i().width}x{i().height} · {i().backend} · OCR {i().ocr ?? "none"} · {i().ops.join(", ")}
              </span>
            )}
          </Show>
          <label class="row small" title="Send a click to the target when you click the image. Input needs an exclusive lease for an agent; the operator may always send it">
            <input type="checkbox" checked={clickToSend()} onChange={(e) => setClickToSend(e.currentTarget.checked)} />
            Click to send
          </label>
          <button class="small" onClick={togglePause} aria-pressed={paused()}>
            {paused() ? "Resume" : "Pause"}
          </button>
          <button class="ghost small" onClick={() => props.onClose()} aria-label="Close the screen">
            ✕
          </button>
        </span>
      </div>
      <Show when={notice()}>{(n) => <p class="small" role="status">{n()}</p>}</Show>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={shot()} fallback={<p class="small muted">{error() ? "No screenshot." : "Taking a screenshot…"}</p>}>
        {(s) => (
          <figure style={{ margin: 0 }}>
            <img
              src={shotSrc(s())}
              alt={`The screen of ${props.target.name}`}
              style={{ "max-width": "100%", "image-rendering": "auto", cursor: clickToSend() ? "crosshair" : "default", border: "1px solid var(--border, #ccc)" }}
              onClick={(e) => void onImageClick(e)}
            />
            <figcaption class="small muted">
              {s().width}x{s().height} of a {s().screen.width}x{s().screen.height} screen{paused() ? " · paused" : ""}
            </figcaption>
          </figure>
        )}
      </Show>
    </div>
  );
}
