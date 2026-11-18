//! Renders a validated route table into the Caddyfile fragment Edge owns.
//!
//! The fragment holds routes only. The host Caddyfile imports it inside its
//! wildcard site, which keeps the certificate (DNS-01), the listener, and the
//! answer for unknown hosts in static configuration:
//!
//! ```caddyfile
//! *.sakala.app {
//!     tls { dns cloudflare {env.CLOUDFLARE_API_TOKEN} }
//!     import /var/lib/sakala-edge/routes.caddy
//!     handle { respond "Not found" 404 }
//! }
//! ```
//!
//! Each project becomes a named `host` matcher and a `handle` block that proxies
//! to its nodes. Caddy keeps the request's `Host` header by default, which the
//! node router needs to pick the project. Every interpolated value was checked
//! by [`crate::table::RoutePolicy`]: hosts are `a-z0-9-.`, upstreams are IP
//! socket addresses, and the project id is a UUID, so none can carry
//! whitespace, braces, quotes, or a newline into the file.

use std::{fmt::Write as _, time::Duration};

use crate::table::{Route, RouteTable};

/// First line of every generated file, so an operator who opens it knows not to
/// edit it by hand.
pub const GENERATED_HEADER: &str =
    "# Managed by sakala-edge. Do not edit: the next sync replaces this file.";
/// Prefix of the matcher name of each route. The rest is the host with dots
/// turned into underscores, which is unique because hosts never hold `_`.
const MATCHER_PREFIX: &str = "@sakala_edge_";

/// How Edge picks a node: the one with the fewest requests in flight. Same
/// policy as the node router uses between replicas.
const LB_POLICY: &str = "least_conn";
/// How long one request keeps looking for a node that answers it. A request that
/// reaches a node that went away is sent to another one inside this window
/// instead of failing. Same window as the node router between replicas, which
/// measured about three seconds for a request on a dead kept-alive connection.
const LB_TRY_DURATION: Duration = Duration::from_secs(5);
/// Pause between two of those attempts.
const LB_TRY_INTERVAL: Duration = Duration::from_millis(250);
/// How long a node that failed a request gets no requests: Caddy's passive
/// health check. Kept short for the same reason as on the node: Caddy does not
/// fall back to a node it marked down, so a failure that hits every node at once
/// makes the site answer 503 for this long.
const FAIL_DURATION: Duration = Duration::from_secs(10);

/// The full file for `table`. The same routes always render the same bytes,
/// which is how Edge tells whether Caddy needs a reload at all. The revision
/// and generation time stay out of the file for that reason: a new revision
/// with the same routes must not reload Caddy. They are in `status.json`.
#[must_use]
pub fn render(table: &RouteTable) -> String {
    let mut content = format!("{GENERATED_HEADER}\n# Routes: {}\n", table.routes().len());
    for route in table.routes() {
        content.push('\n');
        render_route(&mut content, route);
    }
    content
}

fn render_route(content: &mut String, route: &Route) {
    let matcher = format!("{MATCHER_PREFIX}{}", route.host.replace('.', "_"));
    let _ = writeln!(content, "# Project {}", route.project_id);
    let _ = writeln!(content, "{matcher} host {}", route.host);
    let _ = write!(content, "handle {matcher} {{\n\treverse_proxy");
    for upstream in &route.upstreams {
        let _ = write!(content, " {upstream}");
    }
    // One node has nobody to fail over to, and marking it down would only turn
    // one failed request into ten seconds of 503.
    if route.upstreams.len() > 1 {
        let _ = write!(
            content,
            " {{\n\t\tlb_policy {LB_POLICY}\n\t\tlb_try_duration {}\n\t\tlb_try_interval {}\n\t\tfail_duration {}\n\t}}",
            caddy_duration(LB_TRY_DURATION),
            caddy_duration(LB_TRY_INTERVAL),
            caddy_duration(FAIL_DURATION),
        );
    }
    content.push_str("\n}\n");
}

/// A duration in Caddyfile syntax: whole seconds as `5s`, anything finer in
/// milliseconds as `250ms`.
fn caddy_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::table::tests::{PROJECT_A, PROJECT_B, body, policy};

    #[test]
    fn one_upstream_proxies_without_balancing_options() {
        let table = policy()
            .parse(&body(
                json!([{ "host": "blog.sakala.app", "project_id": PROJECT_A,
                         "upstreams": ["10.0.47.201:8180"] }]),
                1,
            ))
            .expect("valid");

        assert_eq!(
            render(&table),
            format!(
                "{GENERATED_HEADER}\n\
                 # Routes: 1\n\
                 \n\
                 # Project {PROJECT_A}\n\
                 @sakala_edge_blog_sakala_app host blog.sakala.app\n\
                 handle @sakala_edge_blog_sakala_app {{\n\
                 \treverse_proxy 10.0.47.201:8180\n\
                 }}\n"
            )
        );
    }

    #[test]
    fn several_upstreams_are_balanced_with_passive_health() {
        let table = policy()
            .parse(&body(
                json!([
                    { "host": "shop.staging.sakala.app", "project_id": PROJECT_B,
                      "upstreams": ["10.0.47.202:8180", "[fd00::1]:8180"] },
                    { "host": "blog.sakala.app", "project_id": PROJECT_A,
                      "upstreams": ["10.0.47.201:8180"] },
                ]),
                0,
            ))
            .expect("valid");

        let rendered = render(&table);

        assert!(rendered.ends_with(&format!(
            "# Project {PROJECT_B}\n\
             @sakala_edge_shop_staging_sakala_app host shop.staging.sakala.app\n\
             handle @sakala_edge_shop_staging_sakala_app {{\n\
             \treverse_proxy 10.0.47.202:8180 [fd00::1]:8180 {{\n\
             \t\tlb_policy least_conn\n\
             \t\tlb_try_duration 5s\n\
             \t\tlb_try_interval 250ms\n\
             \t\tfail_duration 10s\n\
             \t}}\n\
             }}\n"
        )));
        // Sorted by host, whatever order the API sent.
        assert!(rendered.find("blog.sakala.app") < rendered.find("shop.staging.sakala.app"));
        assert!(rendered.contains("\n# Routes: 2\n"));
    }

    #[test]
    fn the_same_table_renders_the_same_bytes() {
        let table = policy()
            .parse(&body(
                json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:80"] }]),
                0,
            ))
            .expect("valid");

        assert_eq!(render(&table), render(&table.clone()));
    }

    #[test]
    fn braces_come_only_from_the_template() {
        let table = policy()
            .parse(&body(
                json!([
                    { "host": "a.sakala.app", "project_id": PROJECT_A,
                      "upstreams": ["10.0.0.1:80", "10.0.0.2:80"] },
                    { "host": "b.sakala.app", "project_id": PROJECT_B, "upstreams": ["10.0.0.3:80"] },
                ]),
                0,
            ))
            .expect("valid");
        let rendered = render(&table);

        // Two handle blocks plus one balancing block, each opened and closed once.
        assert_eq!(rendered.matches('{').count(), 3);
        assert_eq!(rendered.matches('}').count(), 3);
    }

    #[test]
    fn durations_use_caddyfile_units() {
        assert_eq!(caddy_duration(Duration::from_secs(5)), "5s");
        assert_eq!(caddy_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(caddy_duration(Duration::from_millis(1_500)), "1500ms");
    }
}
