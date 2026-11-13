//! The pages a sleeping project's domain answers with.
//!
//! The waking page asks the control plane to wake the project on the first load
//! of a visit, then loads itself again every few seconds, so a visitor reaches
//! the app as soon as it is back. It never reads the control plane's answer: the
//! request is `no-cors`, and the control plane decides whether the project may
//! wake. The reloads stop after a bounded wait, counted in `sessionStorage`, and
//! the page then offers to try again, so a project that cannot wake (a full
//! workspace, a refusing control plane) does not keep every open tab reloading
//! and calling the control plane forever.
//!
//! The unavailable page is static: no script, no wake request, no reload. The
//! control plane asks for it when a visit must not wake the project, such as a
//! suspended one.
//!
//! Everything interpolated here ends up inside a Caddyfile and an HTML page, and
//! Caddy expands `{...}` placeholders in a response body, including ones that
//! read files and environment variables. So the domain and the wake URL are
//! limited to characters that can mean nothing to either, and refused otherwise.
//! The copy and the timings are constants of this module, checked by its tests.

use url::Url;
use uuid::Uuid;

use crate::RuntimeError;

/// A sleeping route for one project, written instead of its serving route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepingRouteSpec {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    pub page: SleepingRoutePage,
}

/// The page a sleeping route answers with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SleepingRoutePage {
    /// Asks `wake_url` to wake the project and reloads until it is back, for a
    /// bounded time.
    Waking { wake_url: Url },
    /// Says the project is not available, and nothing else.
    Unavailable,
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

/// Seconds between two loads of the waking page while the project wakes.
const RELOAD_INTERVAL_SECONDS: u32 = 3;
/// How long one visit keeps reloading before the page says the project could
/// not be woken. A wake that works takes seconds; this leaves room for a slow
/// app start without letting a project that cannot wake loop forever.
const MAX_WAKE_WAIT_SECONDS: u32 = 120;
/// Reloads one visit may make, the wait divided into reload intervals.
const MAX_RELOADS: u32 = MAX_WAKE_WAIT_SECONDS / RELOAD_INTERVAL_SECONDS;
/// Reload interval when the attempts cannot be counted: without JavaScript
/// (`<noscript>`), and when `sessionStorage` refuses to be read or written.
/// Neither case can stop the loop, so it runs slower instead.
const SLOW_RELOAD_SECONDS: u32 = 15;
/// A load more than this long after the visit's previous load starts a new
/// visit, which asks to wake the project again. It is well above the reload
/// interval, so a visit's own reloads never start a new one.
const VISIT_GAP_SECONDS: u32 = 60;
/// Seconds a client should wait before asking the waking page again, its
/// reload interval rounded up. The unavailable page sends no `Retry-After`.
const RETRY_AFTER_SECONDS: u32 = 5;
const MILLIS_PER_SECOND: u32 = 1_000;

const WAKING_TITLE: &str = "Proyek sedang dinyalakan";
const WAKING_TEXT: &str = "Proyek ini sedang tidur karena belum ada pengunjung beberapa waktu. Kami sedang menyalakannya, halaman ini akan memuat ulang sendiri.";
const NOT_WAKING_TITLE: &str = "Proyek belum bisa dinyalakan";
const NOT_WAKING_TEXT: &str =
    "Proyek ini belum bisa dinyalakan sekarang. Coba buka lagi beberapa saat lagi.";
const RETRY_LABEL: &str = "Coba lagi";
const UNAVAILABLE_TITLE: &str = "Proyek tidak tersedia";
const UNAVAILABLE_TEXT: &str = "Proyek ini sedang tidak tersedia.";

/// Closes the heredoc. No line of a page can equal it.
const HEREDOC_MARKER: &str = "SAKALA_SLEEPING_PAGE";

/// Shared by both pages. Small, self-contained, and without external assets.
const STYLE: &str = r#"<style>
html,body{height:100%;margin:0}
body{display:flex;align-items:center;justify-content:center;background:#08413D;color:#fff;font-family:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;line-height:1.5;text-align:center}
main{max-width:32rem;padding:1.5rem}
h1{font-size:1.5rem;margin:0 0 .75rem}
p{margin:0}
button{margin-top:1.25rem;padding:.625rem 1.25rem;border:0;border-radius:.5rem;background:#fff;color:#08413D;font:inherit;font-weight:600;cursor:pointer}
</style>
"#;

/// The waking page's body. The script keeps its count of reloads in
/// `sessionStorage`, under keys only it uses: it asks to wake the project on
/// the visit's first load, reloads every `RELOAD_MS` up to `MAX_RELOADS` times,
/// then swaps in the second `<main>`, whose button starts one more round with
/// a single wake request. Every `{` opens a block whose text up to the next `}`
/// holds a `:` or `;`, so none reads as a Caddy placeholder.
const WAKING_BODY: &str = r#"<main id="waking">
<h1>SAKALA_WAKING_TITLE</h1>
<p>SAKALA_WAKING_TEXT</p>
</main>
<main id="not-waking" hidden>
<h1>SAKALA_NOT_WAKING_TITLE</h1>
<p>SAKALA_NOT_WAKING_TEXT</p>
<button type="button" id="retry">SAKALA_RETRY_LABEL</button>
</main>
<script>
(()=>{
const wakeUrl="SAKALA_WAKE_URL";
const reloadMs=SAKALA_RELOAD_MS,maxReloads=SAKALA_MAX_RELOADS,slowReloadMs=SAKALA_SLOW_RELOAD_MS,visitGapMs=SAKALA_VISIT_GAP_MS;
const reloadsKey="sakala-wake-reloads",lastLoadKey="sakala-wake-last-load";
const read=(key)=>{try{return sessionStorage.getItem(key);}catch(error){return null;}};
const write=(key,value)=>{try{sessionStorage.setItem(key,value);return true;}catch(error){return false;}};
const wake=()=>fetch(wakeUrl,{method:"POST",mode:"no-cors",keepalive:true}).catch(()=>0);
const reloadIn=(ms)=>setTimeout(()=>location.reload(),ms);
const now=Date.now();
const reloads=now-Number(read(lastLoadKey))<=visitGapMs?Number(read(reloadsKey))||0:0;
if(!(write(reloadsKey,String(reloads+1))&&write(lastLoadKey,String(now)))){
const navigation=performance.getEntriesByType("navigation")[0];
if(!navigation||navigation.type!=="reload")wake();
reloadIn(slowReloadMs);
return;
}
if(reloads===0)wake();
if(reloads<maxReloads){reloadIn(reloadMs);return;}
document.getElementById("waking").hidden=true;
const notWaking=document.getElementById("not-waking");
notWaking.hidden=false;
document.title=notWaking.querySelector("h1").textContent;
document.getElementById("retry").addEventListener("click",()=>{
write(reloadsKey,"1");write(lastLoadKey,String(Date.now()));wake();location.reload();
});
})();
</script>
"#;

const UNAVAILABLE_BODY: &str = r#"<main>
<h1>SAKALA_UNAVAILABLE_TITLE</h1>
<p>SAKALA_UNAVAILABLE_TEXT</p>
</main>
"#;

fn millis(seconds: u32) -> String {
    (seconds * MILLIS_PER_SECOND).to_string()
}

/// Puts each value in its slot. Values are module constants or a URL that
/// passed [`page_safe`], so none holds a slot name of its own.
fn fill(template: &str, slots: &[(&str, &str)]) -> String {
    slots
        .iter()
        .fold(template.to_owned(), |page, (slot, value)| {
            page.replace(slot, value)
        })
}

fn document(title: &str, head: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html lang=\"id\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex\">\n\
         <title>{title}</title>\n\
         {head}{STYLE}</head>\n\
         <body>\n\
         {body}</body>\n\
         </html>"
    )
}

fn waking_page(wake_url: &Url) -> String {
    document(
        WAKING_TITLE,
        &format!(
            "<noscript><meta http-equiv=\"refresh\" content=\"{SLOW_RELOAD_SECONDS}\"></noscript>\n"
        ),
        &fill(
            WAKING_BODY,
            &[
                ("SAKALA_WAKING_TITLE", WAKING_TITLE),
                ("SAKALA_WAKING_TEXT", WAKING_TEXT),
                ("SAKALA_NOT_WAKING_TITLE", NOT_WAKING_TITLE),
                ("SAKALA_NOT_WAKING_TEXT", NOT_WAKING_TEXT),
                ("SAKALA_RETRY_LABEL", RETRY_LABEL),
                ("SAKALA_WAKE_URL", wake_url.as_str()),
                ("SAKALA_RELOAD_MS", &millis(RELOAD_INTERVAL_SECONDS)),
                ("SAKALA_MAX_RELOADS", &MAX_RELOADS.to_string()),
                ("SAKALA_SLOW_RELOAD_MS", &millis(SLOW_RELOAD_SECONDS)),
                ("SAKALA_VISIT_GAP_MS", &millis(VISIT_GAP_SECONDS)),
            ],
        ),
    )
}

fn unavailable_page() -> String {
    document(
        UNAVAILABLE_TITLE,
        "",
        &fill(
            UNAVAILABLE_BODY,
            &[
                ("SAKALA_UNAVAILABLE_TITLE", UNAVAILABLE_TITLE),
                ("SAKALA_UNAVAILABLE_TEXT", UNAVAILABLE_TEXT),
            ],
        ),
    )
}

/// The Caddyfile site block of a sleeping route: HTTP 503,
/// `Cache-Control: no-store`, `X-Robots-Tag: noindex`, and the page. Only the
/// waking page sends `Retry-After`.
pub(crate) fn site_block(route: &SleepingRouteSpec) -> Result<String, RuntimeError> {
    if !domain_safe(&route.domain) {
        return Err(RuntimeError::InvalidCommand(
            "the domain of a sleeping route holds characters that are not safe in a route"
                .to_owned(),
        ));
    }
    let (page, retry_after) = match &route.page {
        SleepingRoutePage::Waking { wake_url } => {
            if !page_safe(wake_url.as_str()) {
                return Err(RuntimeError::InvalidCommand(
                    "the wake URL of a sleeping route holds characters that are not safe in the page"
                        .to_owned(),
                ));
            }
            (
                waking_page(wake_url),
                format!("\t\tRetry-After \"{RETRY_AFTER_SECONDS}\"\n"),
            )
        }
        SleepingRoutePage::Unavailable => (unavailable_page(), String::new()),
    };
    let mut block = format!(
        "{domain}:80 {{\n\
         \theader {{\n\
         \t\tContent-Type \"text/html; charset=utf-8\"\n\
         \t\tCache-Control \"no-store\"\n\
         {retry_after}\
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

    use super::{
        HEREDOC_MARKER, MAX_RELOADS, NOT_WAKING_TEXT, NOT_WAKING_TITLE, RETRY_LABEL,
        SleepingRoutePage, SleepingRouteSpec, UNAVAILABLE_TEXT, UNAVAILABLE_TITLE, WAKING_TEXT,
        WAKING_TITLE, site_block, unavailable_page, wake_url, waking_page,
    };

    const WAKE: &str = "https://api.sakala.dev/api/v1/public/projects/portfolio/wake";

    fn route(domain: &str, page: SleepingRoutePage) -> SleepingRouteSpec {
        SleepingRouteSpec {
            project_id: Uuid::nil(),
            deployment_id: Uuid::nil(),
            domain: domain.to_owned(),
            page,
        }
    }

    fn waking(wake: &str) -> SleepingRoutePage {
        SleepingRoutePage::Waking {
            wake_url: Url::parse(wake).expect("URL"),
        }
    }

    fn pages() -> [String; 2] {
        [
            waking_page(&Url::parse(WAKE).expect("URL")),
            unavailable_page(),
        ]
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
    fn the_waking_block_answers_503_with_retry_after_and_no_store() {
        let block = site_block(&route("portfolio.run.sakala.dev", waking(WAKE))).expect("block");

        assert!(
            block.starts_with(
                "portfolio.run.sakala.dev:80 {\n\
                 \theader {\n\
                 \t\tContent-Type \"text/html; charset=utf-8\"\n\
                 \t\tCache-Control \"no-store\"\n\
                 \t\tRetry-After \"5\"\n\
                 \t\tX-Robots-Tag \"noindex\"\n\
                 \t}\n\
                 \trespond <<SAKALA_SLEEPING_PAGE\n\
                 \t\t<!doctype html>\n"
            ),
            "{block}"
        );
        assert!(
            block.ends_with("\t\t</html>\n\t\tSAKALA_SLEEPING_PAGE 503\n}\n"),
            "{block}"
        );
        assert!(block.contains(&format!("\t\tconst wakeUrl=\"{WAKE}\";\n")));
        assert!(block.contains(
            "fetch(wakeUrl,{method:\"POST\",mode:\"no-cors\",keepalive:true}).catch(()=>0);"
        ));
        assert!(
            block.contains("<noscript><meta http-equiv=\"refresh\" content=\"15\"></noscript>")
        );
        assert!(block.contains(&format!("<title>{WAKING_TITLE}</title>")));
    }

    #[test]
    fn the_unavailable_block_answers_a_static_503_without_retry_after() {
        let block = site_block(&route(
            "portfolio.run.sakala.dev",
            SleepingRoutePage::Unavailable,
        ))
        .expect("block");

        assert!(
            block.starts_with(
                "portfolio.run.sakala.dev:80 {\n\
                 \theader {\n\
                 \t\tContent-Type \"text/html; charset=utf-8\"\n\
                 \t\tCache-Control \"no-store\"\n\
                 \t\tX-Robots-Tag \"noindex\"\n\
                 \t}\n\
                 \trespond <<SAKALA_SLEEPING_PAGE\n"
            ),
            "{block}"
        );
        assert!(
            block.ends_with("\t\tSAKALA_SLEEPING_PAGE 503\n}\n"),
            "{block}"
        );
        assert!(!block.contains("Retry-After"), "{block}");
        assert!(block.contains(&format!("\t\t<title>{UNAVAILABLE_TITLE}</title>\n")));
        assert!(block.contains(&format!("\t\t<h1>{UNAVAILABLE_TITLE}</h1>\n")));
        assert!(block.contains(&format!("\t\t<p>{UNAVAILABLE_TEXT}</p>\n")));
        for active in [
            "<script",
            "<noscript",
            "refresh",
            "fetch",
            "wake",
            "reload",
            "<button",
        ] {
            assert!(!block.contains(active), "{active} in {block}");
        }
    }

    #[test]
    fn the_waking_page_caps_its_reloads_and_asks_to_wake_once_per_visit() {
        let [page, _] = pages();

        assert_eq!(MAX_RELOADS, 40);
        assert!(
            page.contains(
                "const reloadMs=3000,maxReloads=40,slowReloadMs=15000,visitGapMs=60000;\n"
            )
        );
        // One wake request: on the visit's first load, or from the button.
        assert_eq!(page.matches("wake()").count(), 3, "{page}");
        assert!(page.contains("\nif(reloads===0)wake();\n"));
        assert!(page.contains("\nif(reloads<maxReloads){reloadIn(reloadMs);return;}\n"));
        // Without storage, only a load that is not a reload asks, and slowly.
        assert!(page.contains(
            "\nif(!navigation||navigation.type!==\"reload\")wake();\nreloadIn(slowReloadMs);\n"
        ));
        // The button resets the count to its own reload, asks once, and reloads.
        assert!(page.contains(
            "write(reloadsKey,\"1\");write(lastLoadKey,String(Date.now()));wake();location.reload();"
        ));
        assert_eq!(page.matches("location.reload()").count(), 2, "{page}");
        // Both states are in the page; the script only switches between them.
        assert!(page.contains(&format!(
            "<main id=\"waking\">\n<h1>{WAKING_TITLE}</h1>\n<p>{WAKING_TEXT}</p>\n</main>\n"
        )));
        assert!(page.contains(&format!(
            "<main id=\"not-waking\" hidden>\n<h1>{NOT_WAKING_TITLE}</h1>\n<p>{NOT_WAKING_TEXT}</p>\n<button type=\"button\" id=\"retry\">{RETRY_LABEL}</button>\n</main>\n"
        )));
        for id in ["waking", "not-waking", "retry"] {
            assert!(page.contains(&format!("getElementById(\"{id}\")")), "{id}");
        }
    }

    #[test]
    fn the_copy_is_plain_text() {
        for copy in [
            WAKING_TITLE,
            WAKING_TEXT,
            NOT_WAKING_TITLE,
            NOT_WAKING_TEXT,
            RETRY_LABEL,
            UNAVAILABLE_TITLE,
            UNAVAILABLE_TEXT,
        ] {
            assert!(
                !copy.contains(['<', '>', '&', '"', '\'', '\\', '{', '}', '\n']),
                "{copy}"
            );
        }
    }

    #[test]
    fn no_page_can_close_its_heredoc_or_name_a_caddy_placeholder() {
        for page in pages() {
            assert!(!page.lines().any(|line| line.trim() == HEREDOC_MARKER));
            // Every slot is filled.
            assert!(!page.contains("SAKALA_"), "{page}");
            assert!(!page.contains("{$"));
            // Every `{` opens a CSS rule, a script block, or an object literal,
            // never a name Caddy would expand such as `{env.X}`, `{file.X}`, or
            // `{host}`: Caddy reads a placeholder up to the next `}`.
            for (index, _) in page.match_indices('{') {
                let after = &page[index + 1..];
                let name = after.split('}').next().unwrap_or_default();
                assert!(
                    name.contains(':') || name.contains(';'),
                    "placeholder-like braces: {{{name}}}"
                );
            }
            assert!(page.starts_with("<!doctype html>\n<html lang=\"id\">\n"));
            assert!(page.ends_with("</body>\n</html>"));
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
            for page in [
                waking("https://api.sakala.dev/w"),
                SleepingRoutePage::Unavailable,
            ] {
                assert!(site_block(&route(domain, page)).is_err(), "{domain:?}");
            }
        }
    }

    #[test]
    fn refuses_a_wake_url_that_is_not_safe_in_the_page() {
        let error = site_block(&route(
            "portfolio.run.sakala.dev",
            waking("https://api.sakala.dev/it's"),
        ))
        .expect_err("a quote cannot reach the script");
        assert!(error.to_string().contains("wake URL"), "{error}");
    }
}
