// The daemon's WebSocket auth contract, in the mock (docs/api.md §1,
// docs/security-review.md F4):
//
// - Bearer: the client offers `bearer.<token>` in `Sec-WebSocket-Protocol`.
//   The daemon checks the token and echoes the same value back.
// - Cookie session: the client offers no subprotocol. The browser sends the
//   session cookie and the page's `Origin`. The daemon takes the upgrade
//   when the cookie is good and the `Origin` is the dashboard's own, and it
//   echoes no subprotocol.
// - `?token=` is refused by default (`TERRARIUM_WS_QUERY_TOKEN=1` turns it
//   on). The mock refuses it, so the mock catches a client that falls back
//   to the query string.
// - A refused upgrade is an HTTP error before the handshake, which a browser
//   reports as `error`, then `close` with code 1006.
//
// The mock's sockets go through the same `AuthSocket` as real mode.

import { AuthSocket, BEARER_PREFIX, type RawSocket } from "../sockets";

/** The token that the mock "daemon" takes. */
export const MOCK_TOKEN = "mock-operator-token";
/** The dashboard origin that the mock "browser" sends on an upgrade. */
export const MOCK_ORIGIN = "http://mock";

export interface Handshake {
  url: string;
  protocols: string[];
  /** The browser sent a good session cookie. */
  cookie?: boolean;
  /** The `Origin` header. A browser always sends it on an upgrade. */
  origin?: string;
}

export interface HandshakeRules {
  token: string;
  /** Accept `?token=`. The daemon's `TERRARIUM_WS_QUERY_TOKEN`. */
  queryToken: boolean;
  /** The origins that a cookie upgrade may come from: the dashboard's own. Empty: no cookie upgrades. */
  origins?: string[];
}

/**
 * The daemon's answer to an upgrade: the subprotocol to echo (`""` when the
 * client offered none and authenticated with the cookie or `?token=`), or
 * null to refuse.
 */
export function acceptHandshake(h: Handshake, rules: HandshakeRules): string | null {
  const offered = h.protocols.filter((p) => p.startsWith(BEARER_PREFIX));
  if (offered.length) return offered.includes(`${BEARER_PREFIX}${rules.token}`) ? `${BEARER_PREFIX}${rules.token}` : null;
  if (h.cookie) return h.origin !== undefined && (rules.origins ?? []).includes(h.origin) ? "" : null;
  const q = new URL(h.url, "ws://mock").searchParams.get("token");
  if (rules.queryToken && q === rules.token) return "";
  return null;
}

export const MOCK_RULES: HandshakeRules = { token: MOCK_TOKEN, queryToken: false, origins: [MOCK_ORIGIN] };

/** What the mock "browser" has when it opens a socket. */
export interface MockSocketAuth {
  /** Bearer mode: the token to offer as the subprotocol. Null: offer none. */
  token: string | null;
  /** The browser holds a good session cookie, so it sends it. */
  cookie: boolean;
}

/** The mock without a client: bearer mode with the mock token, as before sessions. */
export const MOCK_BEARER: MockSocketAuth = { token: MOCK_TOKEN, cookie: false };

/** Open a mock socket the way real mode does: through `AuthSocket`. */
export function mockSocket(
  path: string,
  make: (protocol: string | null, h: Handshake) => RawSocket,
  auth: MockSocketAuth = MOCK_BEARER,
): AuthSocket {
  return new AuthSocket(`ws://mock${path}`, {
    token: auth.token,
    cookie: auth.token === null && auth.cookie,
    make: (url, protocols = []) => {
      const h: Handshake = { url, protocols, cookie: auth.cookie, origin: MOCK_ORIGIN };
      return make(acceptHandshake(h, MOCK_RULES), h);
    },
  });
}
