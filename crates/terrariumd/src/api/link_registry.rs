//! Runtime link boundary. Only explicit declarations admit a link request.
//! Plain axum routes remain usable by operators, but cannot add link access.
use std::collections::HashMap;

use axum::Router;
use axum::http::Method;
use axum::routing::MethodRouter;

use super::Shared;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    OperatorOnly,
    Link,
    Conditional,
    OperatorRead,
    /// Public without credentials, but not part of a link's API authority.
    PublicDenied,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub method: Method,
    pub path: String,
    pub access: Access,
}

#[derive(Default)]
pub struct Registry {
    matcher: matchit::Router<HashMap<Method, Access>>,
    patterns: HashMap<String, HashMap<Method, Access>>,
    pub entries: Vec<Entry>,
}

impl Registry {
    fn new(entries: Vec<Entry>) -> Self {
        let mut paths: HashMap<String, HashMap<Method, Access>> = HashMap::new();
        for e in &entries {
            let previous = paths
                .entry(e.path.clone())
                .or_default()
                .insert(e.method.clone(), e.access);
            assert!(
                previous.is_none(),
                "duplicate link declaration: {} {}",
                e.method,
                e.path
            );
        }
        let mut matcher = matchit::Router::new();
        for (path, methods) in &paths {
            assert!(
                matcher.insert(path.clone(), methods.clone()).is_ok(),
                "link patterns must match axum's syntax"
            );
        }
        Self {
            matcher,
            patterns: paths,
            entries,
        }
    }

    pub fn access(&self, method: &Method, path: &str) -> Option<Access> {
        self.matcher.at(path).ok()?.value.get(method).copied()
    }

    pub fn pattern_access(&self, method: &Method, pattern: &str) -> Option<Access> {
        self.patterns.get(pattern)?.get(method).copied()
    }
}

pub struct RegisteredRouter {
    router: Router<Shared>,
    entries: Vec<Entry>,
}

impl Default for RegisteredRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Router<Shared>> for RegisteredRouter {
    fn from(router: Router<Shared>) -> Self {
        Self {
            router,
            entries: Vec::new(),
        }
    }
}

impl RegisteredRouter {
    pub fn new() -> Self {
        Router::new().into()
    }

    pub fn map_router(mut self, f: impl FnOnce(Router<Shared>) -> Router<Shared>) -> Self {
        self.router = f(self.router);
        self
    }

    /// The method declarations are authoritative, not methods hidden in an
    /// axum MethodRouter or helper. An undeclared method always fails closed.
    pub fn route(
        mut self,
        path: &str,
        methods: MethodRouter<Shared>,
        declarations: &[(Method, Access)],
    ) -> Self {
        self.router = self.router.route(path, methods);
        for (method, access) in declarations {
            self.entries.push(Entry {
                method: method.clone(),
                path: path.to_owned(),
                access: *access,
            });
            if *method == Method::GET && !declarations.iter().any(|(m, _)| *m == Method::HEAD) {
                self.entries.push(Entry {
                    method: Method::HEAD,
                    path: path.to_owned(),
                    access: *access,
                });
            }
        }
        self
    }

    pub fn merge(mut self, other: impl Into<Self>) -> Self {
        let other = other.into();
        self.router = self.router.merge(other.router);
        self.entries.extend(other.entries);
        self
    }

    pub fn nest(mut self, prefix: &str, other: Self) -> Self {
        self.router = self.router.nest(prefix, other.router);
        self.entries.extend(other.entries.into_iter().map(|mut e| {
            e.path = format!("{}{}", prefix.trim_end_matches('/'), e.path);
            e
        }));
        self
    }

    pub fn into_parts(self) -> (Router<Shared>, Registry) {
        (self.router, Registry::new(self.entries))
    }

    pub fn fallback<H, T>(mut self, handler: H) -> Self
    where
        H: axum::handler::Handler<T, Shared>,
        T: 'static,
    {
        self.router = self.router.fallback(handler);
        // The dashboard's files and SPA fallback are public. Supplying a
        // link credential must not turn public routing into an auth bypass.
        for path in ["/", "/{*path}"] {
            for method in [Method::GET, Method::HEAD] {
                self.entries.push(Entry { method, path: path.into(), access: Access::PublicDenied });
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_nests_methods_and_denies_undeclared_methods() {
        let child = RegisteredRouter::new().route(
            "/{id}",
            axum::routing::get(|| async {}),
            &[(Method::GET, Access::Link)],
        );
        let (_, registry) = RegisteredRouter::new()
            .nest("/api/items", child)
            .into_parts();
        assert_eq!(
            registry.access(&Method::GET, "/api/items/one"),
            Some(Access::Link)
        );
        assert_eq!(
            registry.access(&Method::HEAD, "/api/items/one"),
            Some(Access::Link)
        );
        assert_eq!(registry.access(&Method::POST, "/api/items/one"), None);
        assert_eq!(registry.access(&Method::GET, "/items/one"), None);
    }
}
