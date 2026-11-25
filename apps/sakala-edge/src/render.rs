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

use crate::table::{CustomRoutes, Route, RouteTable};

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

/// Snippet every custom domain site imports. The host Caddyfile of the Edge
/// defines it before importing the custom domains file, with the certificate
/// issuer for names outside Sakala's zones (Let's Encrypt over HTTP-01 or
/// TLS-ALPN-01 in `sakala-deployment`). Renaming it breaks every Edge host.
pub const CUSTOM_DOMAIN_TLS_SNIPPET: &str = "sakala_custom_domain_tls";
/// Snippet a custom domain without a workload imports in place of a proxy:
/// the branded not-found page the host Caddyfile also serves for an unknown
/// project, with its status 404 and headers. The site keeps its certificate,
/// so the visitor gets an honest page instead of a TLS error. Defined by the
/// host Caddyfile next to [`CUSTOM_DOMAIN_TLS_SNIPPET`].
pub const NOT_FOUND_SNIPPET: &str = "sakala_not_found";

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
    let _ = writeln!(content, "handle {matcher} {{");
    render_proxy(content, route);
    content.push_str("}\n");
}

/// One indented `reverse_proxy` line, with balancing options when the route
/// has more than one node.
fn render_proxy(content: &mut String, route: &Route) {
    content.push_str("\treverse_proxy");
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
    content.push('\n');
}

/// The custom domains file (ADR-024): one top-level site per domain, sorted
/// by host, each importing [`CUSTOM_DOMAIN_TLS_SNIPPET`] and proxying to the
/// project's nodes like its project route. Caddy obtains and renews each
/// site's certificate itself. A site with no node imports
/// [`NOT_FOUND_SNIPPET`] and nothing else. The site trusts
/// no proxy, so Caddy overwrites the forwarded headers as on the wildcard.
/// Hosts passed [`sakala_agent_runtime::CustomDomainPolicy`], so they are
/// `a-z0-9-.` only.
#[must_use]
pub fn render_custom(custom_routes: &CustomRoutes) -> String {
    let routes = custom_routes.routes();
    let mut content = format!("{GENERATED_HEADER}\n# Custom domains: {}\n", routes.len());
    for route in routes {
        let _ = write!(
            content,
            "\n# Project {}\n{} {{\n\timport {CUSTOM_DOMAIN_TLS_SNIPPET}\n",
            route.project_id, route.host
        );
        if route.upstreams.is_empty() {
            let _ = writeln!(content, "\timport {NOT_FOUND_SNIPPET}");
        } else {
            render_proxy(&mut content, route);
        }
        content.push_str("}\n");
    }
    content
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

    fn custom_table(custom_routes: serde_json::Value) -> RouteTable {
        let mut body: serde_json::Value = serde_json::from_slice(&body(
            json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:80"] }]),
            0,
        ))
        .expect("json");
        body["custom_routes"] = custom_routes;
        let table = policy()
            .parse(&serde_json::to_vec(&body).expect("json"))
            .expect("valid");
        assert_eq!(table.custom_rejection(), None);
        table
    }

    #[test]
    fn custom_domains_become_sorted_sites_that_import_the_tls_snippet() {
        let table = custom_table(json!([
            { "host": "www.contoh.com", "project_id": PROJECT_B,
              "upstreams": ["10.0.47.201:8180", "10.0.47.202:8180"] },
            { "host": "contoh.net", "project_id": PROJECT_A, "upstreams": ["10.0.47.201:8180"] },
            { "host": "kosong.example.org", "project_id": PROJECT_A, "upstreams": [] },
        ]));

        assert_eq!(
            render_custom(table.custom_routes()),
            format!(
                "{GENERATED_HEADER}\n\
                 # Custom domains: 3\n\
                 \n\
                 # Project {PROJECT_A}\n\
                 contoh.net {{\n\
                 \timport sakala_custom_domain_tls\n\
                 \treverse_proxy 10.0.47.201:8180\n\
                 }}\n\
                 \n\
                 # Project {PROJECT_A}\n\
                 kosong.example.org {{\n\
                 \timport sakala_custom_domain_tls\n\
                 \timport sakala_not_found\n\
                 }}\n\
                 \n\
                 # Project {PROJECT_B}\n\
                 www.contoh.com {{\n\
                 \timport sakala_custom_domain_tls\n\
                 \treverse_proxy 10.0.47.201:8180 10.0.47.202:8180 {{\n\
                 \t\tlb_policy least_conn\n\
                 \t\tlb_try_duration 5s\n\
                 \t\tlb_try_interval 250ms\n\
                 \t\tfail_duration 10s\n\
                 \t}}\n\
                 }}\n"
            )
        );
        // The project routes file is unchanged by custom domains.
        assert!(!render(&table).contains("contoh"));
    }

    #[test]
    fn no_custom_domains_render_a_file_without_sites() {
        let table = custom_table(json!([]));
        assert_eq!(
            render_custom(table.custom_routes()),
            format!("{GENERATED_HEADER}\n# Custom domains: 0\n")
        );
    }

    #[test]
    fn durations_use_caddyfile_units() {
        assert_eq!(caddy_duration(Duration::from_secs(5)), "5s");
        assert_eq!(caddy_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(caddy_duration(Duration::from_millis(1_500)), "1500ms");
    }
}
