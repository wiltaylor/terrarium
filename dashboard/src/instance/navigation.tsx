import { A as RouterLink, Navigate as RouterNavigate, useLocation as routerLocation, useNavigate as routerNavigate } from "@solidjs/router";
import type { ComponentProps } from "solid-js";
import type { NavigateOptions } from "@solidjs/router";
import { useInstance } from ".";

/** Non-router links keep download and external-link browser behavior. */
export function InstanceAnchor(props: ComponentProps<"a">) {
  const instance = useInstance();
  const href = () => {
    const path = props.href;
    if (!path) return path;
    if (path.startsWith("/api/")) return path.startsWith(instance.apiHref("/api/")) ? path : instance.apiHref(path);
    return instance.href(path);
  };
  return <a {...props} href={href()} />;
}

export function A(props: ComponentProps<typeof RouterLink>) {
  const instance = useInstance();
  return <RouterLink {...props} href={instance.href(props.href)} />;
}

export function Navigate(props: ComponentProps<typeof RouterNavigate>) {
  const instance = useInstance();
  const target = props.href;
  return <RouterNavigate {...props} href={typeof target === "function" ? (args) => instance.href(target(args)) : instance.href(target)} />;
}

export function useNavigate(): ReturnType<typeof routerNavigate> {
  const navigate = routerNavigate();
  const instance = useInstance();
  return ((to: string | number, options?: Partial<NavigateOptions>) => {
    if (typeof to === "number") navigate(to);
    else if (options === undefined) navigate(instance.href(to));
    else navigate(instance.href(to), options);
  }) as ReturnType<typeof routerNavigate>;
}

/** Route matching inside an instance uses the daemon-relative pathname. */
export function useLocation(): ReturnType<typeof routerLocation> {
  const location = routerLocation();
  const instance = useInstance();
  return new Proxy(location, {
    get: (target, key, receiver) => key === "pathname" ? instance.path(target.pathname) : Reflect.get(target, key, receiver),
  });
}
