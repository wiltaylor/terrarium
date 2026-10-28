// The public key of an `ssh_key` secret (PLAN §16b.6a): the key with a
// copy button, its fingerprint, and the line to add to
// `~/.ssh/authorized_keys` of the user on the target.

import { Show } from "solid-js";
import type { Secret } from "../../api/types";
import { CopyButton } from "../../components/ui";
import { authorizedKeysLine, fingerprint, publicKey } from "./model";

export function PublicKey(props: { secret: Secret }) {
  return (
    <Show when={publicKey(props.secret)}>
      {(pub) => (
        <div class="stack" style={{ gap: "4px" }} aria-label={`Public key of ${props.secret.name}`}>
          <div class="row" style={{ gap: "6px", "flex-wrap": "wrap" }}>
            <span class="small muted">Public key</span>
            <code class="mono small" style={{ "word-break": "break-all" }}>
              {pub()}
            </code>
            <CopyButton text={pub()} label="Copy key" />
          </div>
          <Show when={fingerprint(props.secret)}>
            {(fp) => (
              <div class="small">
                <span class="muted">Fingerprint</span> <code class="mono">{fp()}</code>
              </div>
            )}
          </Show>
          <div class="row" style={{ gap: "6px", "flex-wrap": "wrap" }}>
            <span class="small muted">On the target</span>
            <code class="mono small" style={{ "word-break": "break-all" }}>
              {authorizedKeysLine(pub())}
            </code>
            <CopyButton text={authorizedKeysLine(pub())} label="Copy authorized_keys line" />
          </div>
        </div>
      )}
    </Show>
  );
}
