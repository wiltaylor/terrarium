import { createHttpTransport } from "../api/http";
import { useInstance } from "../instance";

export interface Remote {
  name: string;
  url: string;
  label: string;
  status: string;
  read_only: boolean;
  last_error: string | null;
}

/** Management always belongs to home, including while viewing a remote. */
export function useRemotes() {
  const { auth } = useInstance();
  const transport = createHttpTransport({ getToken: auth.token, getCsrf: auth.csrf });
  return {
    list: () => transport.request<Remote[]>("GET", "/api/remotes", { query: { limit: 64 } }),
    get: (name: string) => transport.request<Remote>("GET", `/api/remotes/${encodeURIComponent(name)}`),
    add: (body: { name: string; url: string; instance_id: string; code: string; label: string }) => transport.request<Remote>("POST", "/api/remotes", { body }),
    remove: (name: string) => transport.request<void>("DELETE", `/api/remotes/${encodeURIComponent(name)}`),
  };
}

export function remoteColour(name: string, storage?: Pick<Storage, "getItem"> | null) {
  const picked = storage?.getItem(`remote-colour:${name}`);
  const hue = Array.from(name).reduce((hash, char) => (hash * 31 + char.charCodeAt(0)) % 360, 0);
  return picked && /^#[0-9a-f]{6}$/i.test(picked) ? picked : `hsl(${hue} 55% 45%)`;
}
