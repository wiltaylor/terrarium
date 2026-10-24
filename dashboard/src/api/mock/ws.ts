// The daemon's WebSocket auth contract, in the mock (docs/security-review.md F4):
//
// - The client offers `bearer.<token>` in `Sec-WebSocket-Protocol`. The
//   daemon checks the token and echoes the same value back.
// - `?token=` still works on the daemon for now, unless
//   `TERRARIUM_WS_QUERY_TOKEN=0`. The mock behaves as with `=0`, so the
//   mock catches a client that falls back to the query string.
// - A refused upgrade is an HTTP error before the handshake, which a browser
//   reports as `error`, then `close` with code 1006.
//
// The mock's sockets go through the same `AuthSocket` as real mode.

import { AuthSocket, BEARER_PREFIX, type RawSocket } from "../sockets";

/** The token the mock dashboard "holds". It never needs the token screen. */
export const MOCK_TOKEN = "mock-operator-token";

export interface Handshake {
  url: string;
  protocols: string[];
}

export interface HandshakeRules {
  token: string;
  /** Accept `?token=`. The daemon's `TERRARIUM_WS_QUERY_TOKEN`. */
  queryToken: boolean;
}

/**
 * The daemon's answer to an upgrade: the subprotocol to echo (`""` when the
 * client offered none and authenticated with `?token=`), or null to refuse.
 */
export function acceptHandshake(h: Handshake, rules: HandshakeRules): string | null {
  const offered = h.protocols.filter((p) => p.startsWith(BEARER_PREFIX));
  if (offered.length) return offered.includes(`${BEARER_PREFIX}${rules.token}`) ? `${BEARER_PREFIX}${rules.token}` : null;
  const q = new URL(h.url, "ws://mock").searchParams.get("token");
  if (rules.queryToken && q === rules.token) return "";
  return null;
}

export const MOCK_RULES: HandshakeRules = { token: MOCK_TOKEN, queryToken: false };

/** Open a mock socket the way real mode does: through `AuthSocket`, with the subprotocol. */
export function mockSocket(path: string, make: (protocol: string | null, h: Handshake) => RawSocket): AuthSocket {
  return new AuthSocket(`ws://mock${path}`, {
    token: MOCK_TOKEN,
    make: (url, protocols = []) => {
      const h = { url, protocols };
      return make(acceptHandshake(h, MOCK_RULES), h);
    },
  });
}
