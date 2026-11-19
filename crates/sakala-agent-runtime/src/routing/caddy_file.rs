use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use tokio::fs;
use uuid::Uuid;

use crate::{
    RuntimeError, RuntimeReporter,
    routing::{
        CaddyReloader, KnownRoutes, RouteIdentity, RouteManager, RouteSpec, SleepingRouteSpec,
        sleeping_page,
    },
};
use sakala_agent_core::ports::RuntimeStaleRoute;

const MANAGED_ROUTE_PREFIX: &str = "# Managed by sakala-agent for project ";
const SLEEPING_MARKER: &str = " (sleeping)";

/// How the router picks a replica: the one with the fewest requests in flight,
/// so a replica stuck on slow requests gets fewer new ones.
const LB_POLICY: &str = "least_conn";
/// How long one request keeps looking for a replica that answers it. A request
/// that reaches a stopped replica is sent to another one within this window
/// instead of failing. Caddy retries only while the window is still open, and a
/// request on a kept-alive connection to a replica that was killed takes about
/// three seconds to fail (measured 2.8 s), so the window leaves room for that.
const LB_TRY_DURATION: Duration = Duration::from_secs(5);
/// Pause between two of those attempts.
const LB_TRY_INTERVAL: Duration = Duration::from_millis(250);
/// How long a replica that failed a request, by refusing the connection or not
/// resolving, gets no requests. This is Caddy's passive health check, the only
/// kind used: an active check in Caddy 2.11 accepts a single status class, while
/// Sakala treats every answer below 500 as healthy, so an app answering 404 on
/// `/` would be taken out of its own route. Kept short because Caddy does not
/// fall back to a replica it marked down, so a failure that hits every replica
/// at once, such as a DNS hiccup, makes the site answer 503 for this long.
const FAIL_DURATION: Duration = Duration::from_secs(10);

/// Request headers that carry exactly one address to the app: the visitor's.
/// The router resolves that address itself as `{client_ip}`: from the
/// `X-Forwarded-For` of host Caddy or Sakala Edge when the connection comes
/// from an address in the router's `trusted_proxies`, read right to left so a
/// value the visitor wrote is skipped, and the connection's own address
/// otherwise, such as a workload calling the router directly. Without this the
/// router appends the edge's address to `X-Forwarded-For`, so an app that trusts
/// only its immediate peer (Laravel `trustProxies(at: '*')`, Express
/// `trust proxy 1`) reports the edge as the client, and a client-supplied
/// `X-Real-IP` reaches the app untouched.
const CLIENT_IP_HEADERS: [&str; 2] = ["X-Forwarded-For", "X-Real-IP"];
/// Caddy's placeholder for the client address it resolved for the request.
const CLIENT_IP_PLACEHOLDER: &str = "{client_ip}";

/// The response header whose values lose their `Domain` attribute.
const SET_COOKIE_HEADER: &str = "Set-Cookie";
/// One `Domain` attribute of a `Set-Cookie` value, in any case, with the `;`
/// before it, optional spaces around the name, and the value up to the next `;`
/// or the end. Removing it makes the cookie host-only. Every project shares the
/// registrable domain `sakala.app` until it is on the Public Suffix List, so a
/// cookie one app sets for `Domain=sakala.app` would otherwise reach every other
/// app. The leading `;` keeps cookie values such as `pref=domain%3Dx` and a
/// cookie named `domain` untouched, and the trailing attributes keep their own
/// `;`. Caddy applies the replacement to each `Set-Cookie` value separately.
/// Written for Go's RE2 inside a double-quoted Caddyfile token, so it holds no
/// `"`, and no `{`, which Caddy would read as a placeholder.
const COOKIE_DOMAIN_ATTRIBUTE_PATTERN: &str = r"(?i);\s*domain\s*=[^;]*";

/// What the first line of a route file says about its owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteOwner {
    /// Written before routes named their deployment.
    Legacy,
    /// Proxies to the deployment's container.
    Serving(Uuid),
    /// Answers with the sleeping page while the deployment's container is stopped.
    Sleeping(Uuid),
}

impl RouteOwner {
    fn deployment_id(self) -> Option<Uuid> {
        match self {
            Self::Legacy => None,
            Self::Serving(deployment_id) | Self::Sleeping(deployment_id) => Some(deployment_id),
        }
    }
}

/// `None` when Sakala does not own the route of `project_id`, including when the
/// line names a deployment in a form this Agent does not write.
fn route_owner(content: &[u8], project_id: Uuid) -> Option<RouteOwner> {
    let first_line = content.split(|byte| *byte == b'\n').next()?;
    let first_line = std::str::from_utf8(first_line).ok()?;
    let rest = first_line
        .strip_prefix(MANAGED_ROUTE_PREFIX)?
        .strip_prefix(&project_id.to_string())?;
    if rest == "." {
        return Some(RouteOwner::Legacy);
    }
    let deployment = rest.strip_prefix(" deployment ")?.strip_suffix('.')?;
    match deployment.strip_suffix(SLEEPING_MARKER) {
        Some(deployment) => Uuid::parse_str(deployment).ok().map(RouteOwner::Sleeping),
        None => Uuid::parse_str(deployment).ok().map(RouteOwner::Serving),
    }
}

fn owner_line(project_id: Uuid, deployment_id: Uuid, sleeping: bool) -> String {
    let marker = if sleeping { SLEEPING_MARKER } else { "" };
    format!("{MANAGED_ROUTE_PREFIX}{project_id} deployment {deployment_id}{marker}.\n")
}

pub struct CaddyFileRouteManager {
    sites_dir: PathBuf,
    reloader: Arc<dyn CaddyReloader>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl CaddyFileRouteManager {
    #[must_use]
    pub fn new(sites_dir: PathBuf, reloader: Arc<dyn CaddyReloader>) -> Self {
        Self {
            sites_dir,
            reloader,
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn replace_route(
        &self,
        project_id: Uuid,
        content: String,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let snapshot = write_route(&self.sites_dir, project_id, content).await?;
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(())
    }
}

struct RouteSnapshot {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    changed: bool,
}

fn serving_route(route: &RouteSpec) -> String {
    let mut content = format!(
        "{}{}:80 {{\n\treverse_proxy",
        owner_line(route.project_id, route.deployment_id, false),
        route.domain,
    );
    for upstream in &route.upstreams {
        let _ = write!(content, " {upstream}:{}", route.port);
    }
    content.push_str(" {\n");
    if route.upstreams.len() > 1 {
        let _ = write!(
            content,
            "\t\tlb_policy {LB_POLICY}\n\t\tlb_try_duration {}\n\t\tlb_try_interval {}\n\t\tfail_duration {}\n",
            caddy_duration(LB_TRY_DURATION),
            caddy_duration(LB_TRY_INTERVAL),
            caddy_duration(FAIL_DURATION),
        );
    }
    for header in CLIENT_IP_HEADERS {
        let _ = writeln!(content, "\t\theader_up {header} {CLIENT_IP_PLACEHOLDER}");
    }
    // Every upstream cookie becomes host-only; the empty replacement deletes
    // the attribute.
    let _ = writeln!(
        content,
        "\t\theader_down {SET_COOKIE_HEADER} \"{COOKIE_DOMAIN_ATTRIBUTE_PATTERN}\" \"\""
    );
    content.push_str("\t}\n}\n");
    content
}

/// A duration as a Caddyfile value: whole seconds as `3s`, anything else in
/// milliseconds.
fn caddy_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn sleeping_route(route: &SleepingRouteSpec) -> Result<String, RuntimeError> {
    Ok(format!(
        "{}{}",
        owner_line(route.project_id, route.deployment_id, true),
        sleeping_page::site_block(route)?
    ))
}

/// Replaces the project's route file through a rename, so Caddy never reads
/// half of one, and keeps the previous content for a rollback.
async fn write_route(
    sites_dir: &Path,
    project_id: Uuid,
    content: String,
) -> Result<RouteSnapshot, RuntimeError> {
    fs::create_dir_all(sites_dir).await?;
    let path = sites_dir.join(format!("{project_id}.Caddyfile"));
    let temporary = sites_dir.join(format!(".{project_id}.Caddyfile.tmp"));
    let previous = match fs::read(&path).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    fs::write(&temporary, content).await?;
    fs::rename(&temporary, &path).await?;

    Ok(RouteSnapshot {
        path,
        previous,
        changed: true,
    })
}

impl RouteSnapshot {
    async fn restore(self) -> Result<(), RuntimeError> {
        match self.previous {
            Some(content) => fs::write(self.path, content).await?,
            None => match fs::remove_file(self.path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }

        Ok(())
    }
}

async fn remove_route(
    sites_dir: &Path,
    identity: RouteIdentity,
) -> Result<RouteSnapshot, RuntimeError> {
    let project_id = identity.project_id;
    let path = sites_dir.join(format!("{project_id}.Caddyfile"));
    let previous = match fs::read(&path).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(content) = &previous {
        let Some(owner) = route_owner(content, project_id) else {
            tracing::warn!(route = %path.display(), "refusing to delete a route Sakala does not own");
            return Err(RuntimeError::Routing(format!(
                "refusing to delete the route of project {project_id} because it is not owned by Sakala"
            )));
        };
        // A serving or sleeping route of another deployment is newer than the
        // command, so it stays. `None` means "still legacy", not a wildcard: a
        // stale legacy discovery may race with a deployment-aware activation.
        if owner.deployment_id() != identity.deployment_id {
            return Ok(RouteSnapshot {
                path,
                previous,
                changed: false,
            });
        }
        fs::remove_file(&path).await?;
    }
    let changed = previous.is_some();
    Ok(RouteSnapshot {
        path,
        previous,
        changed,
    })
}

#[async_trait]
impl RouteManager for CaddyFileRouteManager {
    async fn discover_stale_routes(
        &self,
        known_routes: &KnownRoutes,
    ) -> Result<Vec<RuntimeStaleRoute>, RuntimeError> {
        let mut entries = match fs::read_dir(&self.sites_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut stale_routes = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(project) = name.strip_suffix(".Caddyfile") else {
                continue;
            };
            let Ok(project_id) = Uuid::parse_str(project) else {
                continue;
            };
            let content = fs::read(&path).await?;
            let Some(owner) = route_owner(&content, project_id) else {
                continue;
            };
            let identity = RouteIdentity {
                project_id,
                deployment_id: owner.deployment_id(),
            };
            let known = match owner {
                RouteOwner::Legacy => known_routes
                    .serving
                    .iter()
                    .any(|identity| identity.project_id == project_id),
                RouteOwner::Serving(_) => known_routes.serving.contains(&identity),
                RouteOwner::Sleeping(_) => known_routes.sleeping.contains(&identity),
            };
            if known {
                continue;
            }
            stale_routes.push(RuntimeStaleRoute {
                path: path.display().to_string(),
                project_id,
                deployment_id: identity.deployment_id,
            });
        }
        Ok(stale_routes)
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.replace_route(route.project_id, serving_route(route), reporter)
            .await
    }

    async fn activate_sleeping(
        &self,
        route: &SleepingRouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.replace_route(route.project_id, sleeping_route(route)?, reporter)
            .await
    }

    async fn deactivate(
        &self,
        identity: RouteIdentity,
        reporter: &dyn RuntimeReporter,
    ) -> Result<bool, RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let snapshot = remove_route(&self.sites_dir, identity).await?;
        if !snapshot.changed {
            return Ok(false);
        }
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(true)
    }

    async fn cleanup_stale_routes(
        &self,
        known_routes: &KnownRoutes,
        reporter: &dyn RuntimeReporter,
    ) -> Result<usize, RuntimeError> {
        let stale = self.discover_stale_routes(known_routes).await?;
        let mut cleaned = 0;
        for route in stale {
            if self
                .deactivate(
                    RouteIdentity {
                        project_id: route.project_id,
                        deployment_id: route.deployment_id,
                    },
                    reporter,
                )
                .await?
            {
                cleaned += 1;
            }
        }
        Ok(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn route(upstreams: &[&str]) -> RouteSpec {
        RouteSpec {
            project_id: Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project"),
            deployment_id: Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993")
                .expect("deployment"),
            domain: "portfolio.sakala.app".to_owned(),
            upstreams: upstreams
                .iter()
                .map(|upstream| (*upstream).to_owned())
                .collect(),
            port: 3000,
        }
    }

    #[test]
    fn one_replica_proxies_with_the_client_address_and_makes_cookies_host_only() {
        assert_eq!(
            serving_route(&route(&["app"])),
            "# Managed by sakala-agent for project ff66ed4a-6303-4be6-8ef4-63c28b112680 deployment 4f1f21ef-730d-42d5-a46d-d965353cb993.\n\
             portfolio.sakala.app:80 {\n\
             \treverse_proxy app:3000 {\n\
             \t\theader_up X-Forwarded-For {client_ip}\n\
             \t\theader_up X-Real-IP {client_ip}\n\
             \t\theader_down Set-Cookie \"(?i);\\s*domain\\s*=[^;]*\" \"\"\n\
             \t}\n\
             }\n"
        );
    }

    #[test]
    fn the_cookie_pattern_fits_a_quoted_caddyfile_token() {
        // A `"` would end the token early and a `{` would turn the pattern into
        // a Caddy placeholder, so Caddy would match something else.
        assert!(!COOKIE_DOMAIN_ATTRIBUTE_PATTERN.contains(['"', '{', '}']));
        assert!(COOKIE_DOMAIN_ATTRIBUTE_PATTERN.starts_with("(?i)"));
    }

    #[test]
    fn several_replicas_share_one_balanced_site() {
        assert_eq!(
            serving_route(&route(&["app-r0", "app-r1", "app-r2"])),
            "# Managed by sakala-agent for project ff66ed4a-6303-4be6-8ef4-63c28b112680 deployment 4f1f21ef-730d-42d5-a46d-d965353cb993.\n\
             portfolio.sakala.app:80 {\n\
             \treverse_proxy app-r0:3000 app-r1:3000 app-r2:3000 {\n\
             \t\tlb_policy least_conn\n\
             \t\tlb_try_duration 5s\n\
             \t\tlb_try_interval 250ms\n\
             \t\tfail_duration 10s\n\
             \t\theader_up X-Forwarded-For {client_ip}\n\
             \t\theader_up X-Real-IP {client_ip}\n\
             \t\theader_down Set-Cookie \"(?i);\\s*domain\\s*=[^;]*\" \"\"\n\
             \t}\n\
             }\n"
        );
    }

    #[test]
    fn durations_use_the_shortest_exact_unit() {
        assert_eq!(caddy_duration(Duration::from_secs(10)), "10s");
        assert_eq!(caddy_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(caddy_duration(Duration::from_millis(1_500)), "1500ms");
    }
}
