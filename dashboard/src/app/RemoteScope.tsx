import { useParams, type RouteSectionProps } from "@solidjs/router";
import { createResource, onCleanup, Show, createEffect } from "solid-js";
import { createInstance, InstanceProvider, useInstance } from "../instance";
import { Layout } from "./Layout";
import { useRemotes, remoteColour } from "./remotes";
import { ErrorBanner } from "../components/ui";

export function RemoteScope(props: RouteSectionProps) {
  const params = useParams();
  const remotes = useRemotes();
  const { auth, storage } = useInstance();
  const [remote] = createResource(() => params.remote, (name) => remotes.get(name));
  return <Show when={!remote.error} fallback={<ErrorBanner error={remote.error} />}>
    <Show keyed when={!remote.loading && remote()?.name === params.remote ? remote() : undefined} fallback={<div role="status">Loading remote…</div>}>
      {(r) => {
        const instance = createInstance({ name: r.name, auth });
        onCleanup(instance.dispose);
        const previousTitle = document.title;
        createEffect(() => { document.title = `[${r.name}] Terrarium`; });
        onCleanup(() => { document.title = previousTitle; });
        return <InstanceProvider instance={instance}>
          <div class="remote-banner" style={{ "border-color": remoteColour(r.name, storage) }} role="status">
            <strong>Remote: {r.name}</strong> ({r.url}) · {r.status}
            <Show when={r.read_only}> · read only</Show>
            <Show when={r.status === "version_skew"}> · Version differs from home</Show>
            <a href="/">Return home</a>
          </div>
          <Layout {...props} />
        </InstanceProvider>;
      }}
    </Show>
  </Show>;
}
