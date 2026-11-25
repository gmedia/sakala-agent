use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_protocol::ProjectDomainsUpdate;
use tokio::fs;
use uuid::Uuid;

use crate::{
    RuntimeError, RuntimeReporter,
    routing::{
        CaddyReloader, KnownRoutes, ROUTER_PORT, RouteIdentity, RouteManager, RouteSpec,
        SleepingRouteSpec, site_addresses, sleeping_page,
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

/// What opens the site block after the address line of every route this
/// Agent writes.
const SITE_BLOCK_OPENING: &str = " {";
/// Separates two addresses on the address line.
const ADDRESS_SEPARATOR: &str = ", ";

/// The hosts of a route this Agent wrote: the address line, which follows the
/// owner line, as `<domain>:80[, <custom>:80]... {`. The project's domain comes
/// first. `None` for any other shape, so a file this Agent did not write is
/// never read as a claim on a host.
fn route_hosts(content: &str) -> Option<Vec<String>> {
    let address_line = content.lines().nth(1)?;
    let addresses = address_line.strip_suffix(SITE_BLOCK_OPENING)?;
    let port = format!(":{ROUTER_PORT}");
    addresses
        .split(ADDRESS_SEPARATOR)
        .map(|address| {
            address
                .strip_suffix(port.as_str())
                .filter(|host| {
                    !host.is_empty()
                        && host.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || matches!(byte, b'.' | b'-')
                        })
                })
                .map(str::to_owned)
        })
        .collect()
}

/// `content` with its address line rewritten to serve `custom_domains` after
/// the project's own domain. `None` when the file is not in the shape
/// [`route_hosts`] reads.
fn with_custom_domains(content: &str, custom_domains: &[String]) -> Option<String> {
    let hosts = route_hosts(content)?;
    let (owner, rest) = content.split_once('\n')?;
    let (_, body) = rest.split_once('\n')?;
    Some(format!(
        "{owner}\n{}{SITE_BLOCK_OPENING}\n{body}",
        site_addresses(&hosts[0], custom_domains)
    ))
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

    /// Writes the project's route, after making sure no other project's
    /// route serves one of its custom domains. Both happen under the mutation
    /// lock, so two projects cannot claim one host at the same time.
    async fn replace_route(
        &self,
        project_id: Uuid,
        content: String,
        custom_domains: &[String],
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        ensure_unclaimed(&self.sites_dir, project_id, custom_domains).await?;
        self.write_and_reload(project_id, content, reporter).await
    }

    async fn write_and_reload(
        &self,
        project_id: Uuid,
        content: String,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let snapshot = write_route(&self.sites_dir, project_id, content).await?;
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(())
    }
}

/// Refuses a custom domain that the route of another project already serves.
/// Only routes Sakala owns count; their hosts were checked when written.
async fn ensure_unclaimed(
    sites_dir: &Path,
    project_id: Uuid,
    custom_domains: &[String],
) -> Result<(), RuntimeError> {
    if custom_domains.is_empty() {
        return Ok(());
    }
    for route in managed_routes(sites_dir).await? {
        if route.project_id == project_id {
            continue;
        }
        if let Some(taken) = custom_domains
            .iter()
            .find(|domain| route.hosts.contains(domain))
        {
            tracing::warn!(
                %project_id,
                holder = %route.project_id,
                domain = %taken,
                "refusing a custom domain the route of another project serves"
            );
            return Err(RuntimeError::CustomDomainTaken {
                domain: taken.clone(),
            });
        }
    }
    Ok(())
}

struct RouteSnapshot {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    changed: bool,
}

fn serving_route(route: &RouteSpec) -> String {
    let mut content = format!(
        "{}{}{SITE_BLOCK_OPENING}\n\treverse_proxy",
        owner_line(route.project_id, route.deployment_id, false),
        site_addresses(&route.domain, &route.custom_domains),
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

fn route_path(sites_dir: &Path, project_id: Uuid) -> PathBuf {
    sites_dir.join(format!("{project_id}.Caddyfile"))
}

/// Replaces the project's route file through a rename, so Caddy never reads
/// half of one, and keeps the previous content for a rollback.
async fn write_route(
    sites_dir: &Path,
    project_id: Uuid,
    content: String,
) -> Result<RouteSnapshot, RuntimeError> {
    fs::create_dir_all(sites_dir).await?;
    let path = route_path(sites_dir, project_id);
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
    let path = route_path(sites_dir, project_id);
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

/// A route file Sakala owns, with what its first line says about the owner
/// and the hosts its address line serves (empty for a shape this Agent does
/// not write).
struct ManagedRoute {
    path: PathBuf,
    project_id: Uuid,
    owner: RouteOwner,
    hosts: Vec<String>,
}

impl ManagedRoute {
    /// The hosts after the project's own domain.
    fn custom_domains(&self) -> &[String] {
        self.hosts.get(1..).unwrap_or_default()
    }
}

/// Every route file in `sites_dir` that Sakala owns. Other files, symlinks, and
/// routes whose first line this Agent does not recognise are skipped.
async fn managed_routes(sites_dir: &Path) -> Result<Vec<ManagedRoute>, RuntimeError> {
    let mut entries = match fs::read_dir(sites_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut routes = Vec::new();
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
        let hosts = std::str::from_utf8(&content)
            .ok()
            .and_then(route_hosts)
            .unwrap_or_default();
        routes.push(ManagedRoute {
            path,
            project_id,
            owner,
            hosts,
        });
    }
    Ok(routes)
}

#[async_trait]
impl RouteManager for CaddyFileRouteManager {
    async fn discover_stale_routes(
        &self,
        known_routes: &KnownRoutes,
    ) -> Result<Vec<RuntimeStaleRoute>, RuntimeError> {
        let mut stale_routes = Vec::new();
        for route in managed_routes(&self.sites_dir).await? {
            let project_id = route.project_id;
            let identity = RouteIdentity {
                project_id,
                deployment_id: route.owner.deployment_id(),
            };
            let known = match route.owner {
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
                path: route.path.display().to_string(),
                project_id,
                deployment_id: identity.deployment_id,
            });
        }
        Ok(stale_routes)
    }

    async fn sleeping_routes(&self) -> Result<HashSet<RouteIdentity>, RuntimeError> {
        Ok(managed_routes(&self.sites_dir)
            .await?
            .into_iter()
            .filter_map(|route| match route.owner {
                RouteOwner::Sleeping(deployment_id) => Some(RouteIdentity {
                    project_id: route.project_id,
                    deployment_id: Some(deployment_id),
                }),
                RouteOwner::Legacy | RouteOwner::Serving(_) => None,
            })
            .collect())
    }

    async fn custom_domains(&self, project_id: Uuid) -> Result<Option<Vec<String>>, RuntimeError> {
        let path = route_path(&self.sites_dir, project_id);
        let content = match fs::read(&path).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if route_owner(&content, project_id).is_none() {
            return Ok(None);
        }
        Ok(Some(
            std::str::from_utf8(&content)
                .ok()
                .and_then(route_hosts)
                .map(|hosts| hosts.into_iter().skip(1).collect())
                .unwrap_or_default(),
        ))
    }

    async fn custom_domains_by_project(&self) -> Result<HashMap<Uuid, Vec<String>>, RuntimeError> {
        Ok(managed_routes(&self.sites_dir)
            .await?
            .into_iter()
            .filter(|route| !route.custom_domains().is_empty())
            .map(|route| (route.project_id, route.custom_domains().to_vec()))
            .collect())
    }

    async fn update_custom_domains(
        &self,
        project_id: Uuid,
        custom_domains: &[String],
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectDomainsUpdate, RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let path = route_path(&self.sites_dir, project_id);
        let content = match fs::read(&path).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProjectDomainsUpdate::NoRoute);
            }
            Err(error) => return Err(error.into()),
        };
        if route_owner(&content, project_id).is_none() {
            return Ok(ProjectDomainsUpdate::NoRoute);
        }
        let unreadable = || {
            RuntimeError::Routing(format!(
                "the route of project {project_id} is not in the form this Agent writes; refresh or redeploy it first"
            ))
        };
        let content = String::from_utf8(content).map_err(|_| unreadable())?;
        let hosts = route_hosts(&content).ok_or_else(unreadable)?;
        if hosts[1..] == *custom_domains {
            return Ok(ProjectDomainsUpdate::Unchanged);
        }
        ensure_unclaimed(&self.sites_dir, project_id, custom_domains).await?;
        let updated = with_custom_domains(&content, custom_domains).ok_or_else(unreadable)?;
        self.write_and_reload(project_id, updated, reporter).await?;
        Ok(ProjectDomainsUpdate::Updated)
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.replace_route(
            route.project_id,
            serving_route(route),
            &route.custom_domains,
            reporter,
        )
        .await
    }

    async fn activate_sleeping(
        &self,
        route: &SleepingRouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.replace_route(
            route.project_id,
            sleeping_route(route)?,
            &route.custom_domains,
            reporter,
        )
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
            custom_domains: Vec::new(),
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
    fn custom_domains_are_extra_site_addresses_after_the_project_domain() {
        let route = RouteSpec {
            custom_domains: vec!["contoh.com".to_owned(), "www.contoh.com".to_owned()],
            ..route(&["app"])
        };
        let content = serving_route(&route);

        assert!(content.contains(
            "\nportfolio.sakala.app:80, contoh.com:80, www.contoh.com:80 {\n\treverse_proxy app:3000 {\n"
        ));
        assert_eq!(
            route_hosts(&content),
            Some(vec![
                "portfolio.sakala.app".to_owned(),
                "contoh.com".to_owned(),
                "www.contoh.com".to_owned()
            ])
        );
        // Without custom domains the file is byte for byte what it was before.
        assert_eq!(
            route_hosts(&serving_route(&route_spec_without_custom())),
            Some(vec!["portfolio.sakala.app".to_owned()])
        );
    }

    fn route_spec_without_custom() -> RouteSpec {
        route(&["app"])
    }

    #[test]
    fn rewriting_the_custom_domains_keeps_everything_else() {
        let before = serving_route(&route(&["app-r0", "app-r1"]));
        let after =
            with_custom_domains(&before, &["www.contoh.com".to_owned()]).expect("our shape");

        assert_eq!(
            after,
            before.replacen(
                "portfolio.sakala.app:80 {",
                "portfolio.sakala.app:80, www.contoh.com:80 {",
                1
            )
        );
        assert_eq!(with_custom_domains(&after, &[]).expect("our shape"), before);
    }

    #[test]
    fn only_the_shape_this_agent_writes_is_read_as_hosts() {
        for content in [
            "",
            "# Managed by sakala-agent for project x.\n",
            "# owner\nportfolio.sakala.app {\n}\n",
            "# owner\nportfolio.sakala.app:80,www.contoh.com:80 {\n}\n",
            "# owner\n:80 {\n}\n",
            "# owner\nportfolio.sakala.app:443 {\n}\n",
        ] {
            assert_eq!(route_hosts(content), None, "{content:?}");
        }
    }

    #[test]
    fn durations_use_the_shortest_exact_unit() {
        assert_eq!(caddy_duration(Duration::from_secs(10)), "10s");
        assert_eq!(caddy_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(caddy_duration(Duration::from_millis(1_500)), "1500ms");
    }
}
