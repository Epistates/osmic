//! Extra routes mounted next to the built-in endpoints.

use axum::Router;

use crate::error::ServeError;

/// First path segments of the built-in endpoints.
const RESERVED: &[&str] = &[
    "tiles",
    "tiles.json",
    "style.json",
    "metadata",
    "viewer.js",
    "viewer.css",
    "healthz",
    "readyz",
];

/// Routers to serve under their own path prefixes, behind the same
/// middleware (timeout, load shedding, CORS, tracing) as the built-in
/// endpoints.
///
/// With [`TileServerPlugin`](crate::TileServerPlugin), other plugins add
/// routes through this resource:
///
/// ```
/// use axum::{Router, routing::get};
/// use osmic_app::{App, BoxError, Plugin};
/// use osmic_serve::{ServerRoutes, TileServerPlugin};
///
/// struct VersionPlugin;
/// impl Plugin for VersionPlugin {
///     fn build(&self, app: &mut App) -> Result<(), BoxError> {
///         app.add_plugin(TileServerPlugin::new("tiles.pmtiles"));
///         Ok(())
///     }
///     fn finish(&self, app: &mut App) -> Result<(), BoxError> {
///         app.resource_mut::<ServerRoutes>()?
///             .nest("/version", Router::new().route("/", get(|| async { "1.0" })));
///         Ok(())
///     }
/// }
/// ```
#[derive(Default)]
pub struct ServerRoutes {
    routes: Vec<(String, Router)>,
}

impl ServerRoutes {
    /// Serve `router` under `prefix` (for example `/api`). Prefixes are
    /// checked when the server is assembled: they must be absolute, made of
    /// unreserved URL characters, not shadow a built-in endpoint and not
    /// overlap one another.
    pub fn nest(&mut self, prefix: impl Into<String>, router: Router) -> &mut Self {
        self.routes.push((prefix.into(), router));
        self
    }

    /// Whether no routes were added.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Mount every router onto `base`.
    pub(crate) fn mount(self, mut base: Router) -> Result<Router, ServeError> {
        for (i, (prefix, _)) in self.routes.iter().enumerate() {
            validate_prefix(prefix)?;
            if let Some((other, _)) = self.routes[..i]
                .iter()
                .find(|(other, _)| overlaps(prefix, other))
            {
                return Err(invalid(prefix, &format!("overlaps {other:?}")));
            }
        }
        for (prefix, router) in self.routes {
            base = base.nest(&prefix, router);
        }
        Ok(base)
    }
}

impl std::fmt::Debug for ServerRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.routes.iter().map(|(p, _)| p))
            .finish()
    }
}

fn invalid(prefix: &str, reason: &str) -> ServeError {
    ServeError::InvalidConfig {
        field: "routes",
        reason: format!("prefix {prefix:?} {reason}"),
    }
}

fn validate_prefix(prefix: &str) -> Result<(), ServeError> {
    let Some(path) = prefix.strip_prefix('/') else {
        return Err(invalid(prefix, "must start with '/'"));
    };
    let segments: Vec<&str> = path.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(invalid(prefix, "must not be '/' or contain empty segments"));
    }
    let unreserved = |c: char| c.is_ascii_alphanumeric() || "-._~".contains(c);
    if !segments.iter().all(|s| s.chars().all(unreserved)) {
        return Err(invalid(
            prefix,
            "may only contain A-Z a-z 0-9 - . _ ~ and '/'",
        ));
    }
    if RESERVED.contains(&segments[0]) {
        return Err(invalid(prefix, "shadows a built-in endpoint"));
    }
    Ok(())
}

/// Whether one prefix equals the other or contains it as whole segments.
fn overlaps(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.strip_prefix(short)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(prefixes: &[&str]) -> Result<Router, ServeError> {
        let mut routes = ServerRoutes::default();
        for p in prefixes {
            routes.nest(*p, Router::new());
        }
        routes.mount(Router::new())
    }

    #[test]
    fn accepts_distinct_prefixes() {
        assert!(mount(&["/api", "/admin/v1", "/apis", "/x.y-z_~"]).is_ok());
    }

    #[test]
    fn rejects_bad_prefixes() {
        for bad in [
            "api",
            "/",
            "",
            "/api/",
            "/a//b",
            "/{id}",
            "/*",
            "/a b",
            "/tiles",
            "/tiles/x",
            "/healthz",
            "/style.json",
        ] {
            let err = mount(&[bad]).expect_err(bad);
            assert!(err.to_string().contains("routes"), "{bad}: {err}");
        }
    }

    #[test]
    fn rejects_overlapping_prefixes() {
        assert!(mount(&["/api", "/api"]).is_err());
        assert!(mount(&["/api/v1", "/api"]).is_err());
        assert!(mount(&["/api", "/api/v1"]).is_err());
    }
}
