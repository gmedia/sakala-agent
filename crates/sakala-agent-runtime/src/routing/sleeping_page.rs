//! The page a sleeping project's domain answers with.
//!
//! The page asks the control plane to wake the project once per load, then loads
//! itself again, so a visitor reaches the app as soon as it is back. It never
//! reads the control plane's answer: the request is `no-cors`, and the control
//! plane decides whether the project may wake (a suspended one may not).
//!
//! Everything interpolated here ends up inside a Caddyfile and an HTML page, and
//! Caddy expands `{...}` placeholders in a response body, including ones that
//! read files and environment variables. So the domain and the wake URL are
//! limited to characters that can mean nothing to either, and refused otherwise.

use url::Url;
use uuid::Uuid;

use crate::RuntimeError;

/// A sleeping route for one project, written instead of its serving route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepingRouteSpec {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    pub wake_url: Url,
}

/// The control plane's public wake endpoint for the project with `slug`, under
/// the API URL the agent talks to. Visitors' browsers call it, so that URL must
/// be reachable from the internet for a sleeping page to wake its project.
pub fn wake_url(api_url: &str, slug: &str) -> Result<Url, RuntimeError> {
    let invalid = |reason: &str| {
        RuntimeError::InvalidCommand(format!(
            "cannot build the wake URL of a sleeping project: {reason}"
        ))
    };
    let slug_is_label = !slug.is_empty()
        && slug.len() <= 63
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !slug_is_label {
        return Err(invalid("the project label is not a DNS label"));
    }
    let base = Url::parse(api_url.trim_end_matches('/'))
        .map_err(|_| invalid("SAKALA_API_URL is not a URL"))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(invalid(
            "SAKALA_API_URL must be an http or https URL without credentials, query, or fragment",
        ));
    }
    let url = Url::parse(&format!(
        "{}/api/v1/public/projects/{slug}/wake",
        base.as_str().trim_end_matches('/')
    ))
    .map_err(|_| invalid("SAKALA_API_URL does not combine with the wake path"))?;
    if !page_safe(url.as_str()) {
        return Err(invalid(
            "SAKALA_API_URL holds characters that are not safe in the page",
        ));
    }
    Ok(url)
}

/// Characters a URL may keep and still be pasted into a Caddyfile heredoc, a
/// Caddy response body, an HTML document, and a double-quoted JavaScript string
/// without meaning anything there.
fn page_safe(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b':' | b'/' | b'.' | b'-' | b'_' | b'~' | b'%' | b'[' | b']'
                )
        })
}

fn domain_safe(domain: &str) -> bool {
    !domain.is_empty()
        && domain.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
}

/// Closes the heredoc. No line of the page can equal it.
const HEREDOC_MARKER: &str = "SAKALA_SLEEPING_PAGE";
const WAKE_URL_SLOT: &str = "SAKALA_WAKE_URL";
/// Seconds a client should wait before asking again, the page's reload interval
/// rounded up.
const RETRY_AFTER_SECONDS: u32 = 5;

/// Small, self-contained, and without external assets. Braces appear only in
/// CSS rules and one object literal, none of them a Caddy placeholder name.
const PAGE: &str = r#"<!doctype html>
<html lang="id">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex">
<title>Proyek sedang dinyalakan</title>
<noscript><meta http-equiv="refresh" content="5"></noscript>
<style>
html,body{height:100%;margin:0}
body{display:flex;align-items:center;justify-content:center;background:#08413D;color:#fff;font-family:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;line-height:1.5;text-align:center}
main{max-width:32rem;padding:1.5rem}
h1{font-size:1.5rem;margin:0 0 .75rem}
p{margin:0}
</style>
</head>
<body>
<main>
<h1>Proyek sedang dinyalakan</h1>
<p>Proyek ini sedang tidur karena belum ada pengunjung beberapa waktu. Kami sedang menyalakannya, halaman ini akan memuat ulang sendiri.</p>
</main>
<script>
fetch("SAKALA_WAKE_URL",{method:"POST",mode:"no-cors",keepalive:true}).catch(()=>0);
setTimeout(()=>location.reload(),3000);
</script>
</body>
</html>"#;

/// The Caddyfile site block of a sleeping route: HTTP 503, `Retry-After`,
/// `Cache-Control: no-store`, and the page.
pub(crate) fn site_block(route: &SleepingRouteSpec) -> Result<String, RuntimeError> {
    if !domain_safe(&route.domain) {
        return Err(RuntimeError::InvalidCommand(
            "the domain of a sleeping route holds characters that are not safe in a route"
                .to_owned(),
        ));
    }
    if !page_safe(route.wake_url.as_str()) {
        return Err(RuntimeError::InvalidCommand(
            "the wake URL of a sleeping route holds characters that are not safe in the page"
                .to_owned(),
        ));
    }
    let page = PAGE.replace(WAKE_URL_SLOT, route.wake_url.as_str());
    let mut block = format!(
        "{domain}:80 {{\n\
         \theader {{\n\
         \t\tContent-Type \"text/html; charset=utf-8\"\n\
         \t\tCache-Control \"no-store\"\n\
         \t\tRetry-After \"{RETRY_AFTER_SECONDS}\"\n\
         \t\tX-Robots-Tag \"noindex\"\n\
         \t}}\n\
         \trespond <<{HEREDOC_MARKER}\n",
        domain = route.domain
    );
    for line in page.lines() {
        block.push('\t');
        block.push('\t');
        block.push_str(line);
        block.push('\n');
    }
    block.push_str(&format!("\t\t{HEREDOC_MARKER} 503\n}}\n"));
    Ok(block)
}

#[cfg(test)]
mod tests {
    use url::Url;
    use uuid::Uuid;

    use super::{HEREDOC_MARKER, PAGE, SleepingRouteSpec, site_block, wake_url};

    fn route(domain: &str, wake: &str) -> SleepingRouteSpec {
        SleepingRouteSpec {
            project_id: Uuid::nil(),
            deployment_id: Uuid::nil(),
            domain: domain.to_owned(),
            wake_url: Url::parse(wake).expect("URL"),
        }
    }

    #[test]
    fn builds_the_public_wake_endpoint_under_the_api_url() {
        assert_eq!(
            wake_url("https://api.staging.sakala.dev", "portfolio")
                .expect("wake URL")
                .as_str(),
            "https://api.staging.sakala.dev/api/v1/public/projects/portfolio/wake"
        );
        assert_eq!(
            wake_url("http://localhost:8000/", "a-1")
                .expect("wake URL")
                .as_str(),
            "http://localhost:8000/api/v1/public/projects/a-1/wake"
        );
        assert_eq!(
            wake_url("https://example.ac.id/sakala/", "web")
                .expect("wake URL")
                .as_str(),
            "https://example.ac.id/sakala/api/v1/public/projects/web/wake"
        );
    }

    #[test]
    fn refuses_slugs_and_api_urls_that_could_escape_the_page() {
        for slug in [
            "",
            "-a",
            "a-",
            "A",
            "a.b",
            "a/b",
            "a\"b",
            "{env.HOME}",
            "a b",
        ] {
            assert!(wake_url("https://api.sakala.dev", slug).is_err(), "{slug}");
        }
        for api in [
            "ftp://api.sakala.dev",
            "https://user:secret@api.sakala.dev",
            "https://api.sakala.dev/?a=1",
            "https://api.sakala.dev/#x",
            "not a url",
            "file:///etc/passwd",
        ] {
            assert!(wake_url(api, "web").is_err(), "{api}");
        }
        // The URL parser percent-encodes these, so they never reach the page.
        let encoded =
            wake_url("https://api.sakala.dev/{env.HOME}\"<>`", "web").expect("encoded path");
        for character in ['{', '}', '"', '<', '>', '`'] {
            assert!(!encoded.as_str().contains(character), "{encoded}");
        }
        // It keeps a quote, which the page check refuses.
        assert!(wake_url("https://api.sakala.dev/it's", "web").is_err());
    }

    #[test]
    fn the_block_answers_503_with_retry_after_and_no_store() {
        let block = site_block(&route(
            "portfolio.run.sakala.dev",
            "https://api.sakala.dev/api/v1/public/projects/portfolio/wake",
        ))
        .expect("block");

        assert!(
            block.starts_with("portfolio.run.sakala.dev:80 {\n"),
            "{block}"
        );
        assert!(block.contains("\t\tRetry-After \"5\"\n"));
        assert!(block.contains("\t\tCache-Control \"no-store\"\n"));
        assert!(block.contains("\t\tContent-Type \"text/html; charset=utf-8\"\n"));
        assert!(
            block.ends_with("\t\tSAKALA_SLEEPING_PAGE 503\n}\n"),
            "{block}"
        );
        assert!(block.contains(
            "fetch(\"https://api.sakala.dev/api/v1/public/projects/portfolio/wake\",{method:\"POST\",mode:\"no-cors\",keepalive:true})"
        ));
        assert!(block.contains("setTimeout(()=>location.reload(),3000)"));
        assert!(block.contains("<noscript><meta http-equiv=\"refresh\" content=\"5\"></noscript>"));
        assert!(!block.contains("SAKALA_WAKE_URL"));
    }

    #[test]
    fn the_page_cannot_close_its_heredoc_or_name_a_caddy_placeholder() {
        assert!(!PAGE.lines().any(|line| line.trim() == HEREDOC_MARKER));
        assert!(!PAGE.contains("{$"));
        // Every `{` opens a CSS rule or the fetch options, never a name Caddy
        // would expand such as `{env.X}`, `{file.X}`, or `{host}`.
        for (index, _) in PAGE.match_indices('{') {
            let after = &PAGE[index + 1..];
            let name = after.split('}').next().unwrap_or_default();
            assert!(
                name.contains(':') || name.contains(';'),
                "placeholder-like braces: {{{name}}}"
            );
        }
    }

    #[test]
    fn refuses_a_domain_that_could_break_the_route() {
        for domain in [
            "",
            "Portfolio.run.sakala.dev",
            "a.run.sakala.dev {",
            "a\nb",
            "{env.X}",
        ] {
            assert!(
                site_block(&route(domain, "https://api.sakala.dev/w")).is_err(),
                "{domain:?}"
            );
        }
    }
}
