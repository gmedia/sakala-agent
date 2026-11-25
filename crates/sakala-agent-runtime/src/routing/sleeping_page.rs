//! The pages a sleeping project's domain answers with.
//!
//! The waking page asks the control plane to wake the project, then loads
//! itself again every few seconds, so a visitor reaches the app as soon as it is
//! back. It never reads the control plane's answer: the request is `no-cors`,
//! and the control plane decides whether the project may wake. Because the page
//! cannot tell whether a request arrived, it asks again every
//! [`WAKE_REPEAT_SECONDS`] while it waits; the control plane keeps one wake per
//! project, so asking again never starts a second one. The reloads stop after a
//! bounded wait, counted in `sessionStorage`, and the page then offers to try
//! again, so a project that cannot wake (a full workspace, a refusing control
//! plane) does not keep every open tab reloading and calling the control plane
//! forever. Without JavaScript the page reloads slowly and offers a plain form
//! that sends the same request.
//!
//! The unavailable page is static: no script, no wake request, no reload. The
//! control plane asks for it when a visit must not wake the project, such as a
//! suspended one.
//!
//! Both pages are self-contained: styles, script, and the Sakala mark are
//! inline, nothing is loaded from elsewhere, and a `Content-Security-Policy`
//! allows exactly those inline blocks (by hash) and requests to the wake
//! endpoint's origin. When the Console URL is known, both pages link the
//! project's owner to it.
//!
//! Everything interpolated here ends up inside a Caddyfile and an HTML page, and
//! Caddy expands `{...}` placeholders in a response body, including ones that
//! read files and environment variables. So the domain, the wake URL, and the
//! Console URL are limited to characters that can mean nothing to either, and
//! refused otherwise. No project name or other text a user wrote is ever put in
//! a page. The copy and the timings are constants of this module, checked by
//! its tests.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest as _, Sha256};
use url::Url;
use uuid::Uuid;

use crate::RuntimeError;

/// A sleeping route for one project, written instead of its serving route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepingRouteSpec {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    /// Further hosts that show the same page, as on the serving route. The
    /// page wakes the project by its slug, taken from `domain`, so a visit
    /// through a custom domain wakes it too.
    pub custom_domains: Vec<String>,
    pub page: SleepingRoutePage,
    /// Where the project's owner manages it (`SAKALA_PUBLIC_CONSOLE_URL`). The
    /// page names the Console without a link when this is unknown.
    pub console_url: Option<Url>,
}

/// The page a sleeping route answers with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SleepingRoutePage {
    /// Asks `wake_url` to wake the project and reloads until it is back, for a
    /// bounded time.
    Waking { wake_url: Url },
    /// Says the project is not available, and where its owner can see why.
    Unavailable,
}

/// Checks the base URL browsers reach the control plane at
/// (`SAKALA_PUBLIC_API_URL`, or `SAKALA_API_URL` when unset): an absolute
/// `http` or `https` URL without credentials, query, or fragment, made only of
/// characters that are safe in the sleeping page. A path is kept as the prefix
/// the API is served under. The error is the reason, for the caller to name
/// its setting.
pub fn check_public_api_url(api_url: &str) -> Result<Url, &'static str> {
    check_public_base_url(api_url)
}

/// Checks the Console URL a sleeping page links the project's owner to
/// (`SAKALA_PUBLIC_CONSOLE_URL`), with the same rules as
/// [`check_public_api_url`]. A path is kept as the prefix the Console is
/// served under.
pub fn check_public_console_url(console_url: &str) -> Result<Url, &'static str> {
    check_public_base_url(console_url)
}

fn check_public_base_url(value: &str) -> Result<Url, &'static str> {
    let base = Url::parse(value.trim_end_matches('/')).map_err(|_| "is not a URL")?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("must be an http or https URL without credentials, query, or fragment");
    }
    if !page_safe(base.as_str()) {
        return Err("holds characters that are not safe in the sleeping page");
    }
    Ok(base)
}

/// The control plane's public wake endpoint for the project with `slug`, under
/// the public API URL. Visitors' browsers call it, so that URL must be
/// reachable from the internet for a sleeping page to wake its project.
pub fn wake_url(public_api_url: &str, slug: &str) -> Result<Url, RuntimeError> {
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
    let base = check_public_api_url(public_api_url)
        .map_err(|reason| invalid(&format!("the public API URL {reason}")))?;
    let url = Url::parse(&format!(
        "{}/api/v1/public/projects/{slug}/wake",
        base.as_str().trim_end_matches('/')
    ))
    .map_err(|_| invalid("the public API URL does not combine with the wake path"))?;
    if !page_safe(url.as_str()) {
        return Err(invalid(
            "the public API URL holds characters that are not safe in the page",
        ));
    }
    Ok(url)
}

/// Characters a URL may keep and still be pasted into a Caddyfile heredoc, a
/// Caddy response body or header, an HTML attribute, and a double-quoted
/// JavaScript string without meaning anything there.
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
/// not be woken. Measured wakes, from the request to the node reporting the
/// app ready: 3 to 11 seconds in production (7 wakes, 19 to 23 Nov 2026) and 5
/// to 35 seconds on the smaller staging host (14 wakes). This leaves room for a
/// slow app start without letting a project that cannot wake loop forever.
const MAX_WAKE_WAIT_SECONDS: u32 = 120;
const SECONDS_PER_MINUTE: u32 = 60;
/// Reloads one visit may make, the wait divided into reload intervals.
const MAX_RELOADS: u32 = MAX_WAKE_WAIT_SECONDS / RELOAD_INTERVAL_SECONDS;
/// How often a waiting page asks to wake the project again. The page cannot
/// see whether a request arrived, and one lost request (a network change, a
/// control plane busy with a build) used to leave the page reloading for the
/// whole wait with nothing waking the project. Asking again is safe because the
/// control plane returns the wake already queued instead of a second one. It
/// stays under the control plane's limit of 6 requests a minute per visitor
/// address and project (`sakala.rate_limits.project_wake`).
const WAKE_REPEAT_SECONDS: u32 = 15;
/// Reloads between two wake requests.
const WAKE_REPEAT_RELOADS: u32 = WAKE_REPEAT_SECONDS / RELOAD_INTERVAL_SECONDS;
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

/// How long a wake usually takes, as visitors and owners read it. The Console
/// shows the same words for a sleeping project (`WAKE_ESTIMATE` in
/// sakala-console), so both say the same thing. Every measured wake (see
/// [`MAX_WAKE_WAIT_SECONDS`]) fits well inside it; an app that starts slowly
/// can take longer, which "usually" leaves room for.
const WAKE_ESTIMATE: &str = "kurang dari satu menit";

const WAKING_TITLE: &str = "Aplikasi ini sedang dinyalakan";
const WAKING_TEXT: &str = "Aplikasi ini tidur karena beberapa waktu tidak dikunjungi. Biasanya siap dalam SAKALA_WAKE_ESTIMATE, dan halaman ini akan terbuka sendiri.";
const NO_SCRIPT_TEXT: &str =
    "JavaScript di browser-mu tidak aktif, jadi tekan tombol ini untuk menyalakannya.";
const NO_SCRIPT_LABEL: &str = "Nyalakan aplikasi";
const NO_SCRIPT_FRAME_TITLE: &str = "Permintaan menyalakan aplikasi";
const WAKING_OWNER: &str = "Kalau ini proyekmu, statusnya juga terlihat di SAKALA_CONSOLE.";
const NOT_WAKING_TITLE: &str = "Aplikasi ini belum bisa dinyalakan";
/// `SAKALA_WAIT_MINUTES` is [`MAX_WAKE_WAIT_SECONDS`] in minutes.
const NOT_WAKING_TEXT: &str =
    "Sudah ditunggu SAKALA_WAIT_MINUTES menit, tetapi aplikasinya belum menyala.";
const NOT_WAKING_OWNER: &str = "Kalau ini proyekmu, sebabnya bisa dilihat di SAKALA_CONSOLE.";
const RETRY_LABEL: &str = "Coba lagi";
const UNAVAILABLE_TITLE: &str = "Aplikasi ini sedang tidak tersedia";
const UNAVAILABLE_TEXT: &str = "Untuk sementara aplikasi ini tidak bisa dibuka.";
const UNAVAILABLE_OWNER: &str = "Kalau ini proyekmu, alasannya bisa dilihat di SAKALA_CONSOLE.";
const CONSOLE_NAME: &str = "Console Sakala";
const HOSTED_BY: &str = "Di-host di Sakala";

/// Closes the heredoc. No line of a page can equal it.
const HEREDOC_MARKER: &str = "SAKALA_SLEEPING_PAGE";

/// The rules of the one `<style>` block both pages share. Colors are the brand
/// kit's deep teal family (PANDUAN-MEREK.md): white on `#08413D` is 11.45:1,
/// `#CCFBF1` 10.16:1, and `#B5D5D2` 7.31:1. Fonts are the system's, because a
/// page that loads nothing cannot carry Montserrat or Inter. The progress bar
/// stands still under `prefers-reduced-motion: reduce`.
const STYLE: &str = "
*{box-sizing:border-box}
html{height:100%}
body{min-height:100%;margin:0;display:flex;flex-direction:column;background:#08413D;color:#fff;font-family:system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif;line-height:1.5}
main,footer{width:100%;max-width:34rem;margin:0 auto;padding-inline:1.5rem}
main{flex:1;display:flex;flex-direction:column;justify-content:center;padding-block:3rem 2rem}
h1{margin:0 0 .75rem;font-size:1.75rem;line-height:1.25;font-weight:700;text-wrap:balance}
p{margin:0 0 1rem}
.lead{font-size:1.0625rem}
.hint{color:#B5D5D2;font-size:.9375rem}
a{color:#CCFBF1;text-decoration:underline;text-underline-offset:.2em}
a:hover{text-decoration-thickness:2px}
button{margin:.25rem 0 1.5rem;min-height:2.75rem;padding:.625rem 1.25rem;border:0;border-radius:.5rem;background:#fff;color:#08413D;font:inherit;font-weight:600;cursor:pointer}
:focus-visible{outline:2px solid #CCFBF1;outline-offset:3px}
.bar{width:12rem;max-width:100%;height:4px;margin:0 0 1.5rem;border-radius:2px;background:#0B4D48;overflow:hidden}
.bar::before{content:\"\";display:block;width:40%;height:100%;border-radius:2px;background:#CCFBF1;animation:sakala-bar 1.6s ease-in-out infinite}
@keyframes sakala-bar{from{transform:translateX(-100%)}to{transform:translateX(250%)}}
@media (prefers-reduced-motion:reduce){.bar::before{width:100%;opacity:.6;animation:none}}
footer{display:flex;align-items:center;gap:.5rem;padding-block:1rem 1.5rem;color:#B5D5D2;font-size:.875rem}
footer svg{flex:none;width:auto;height:24px}
";

/// The Sakala mark from the brand kit (`logo/svg-putih/sakala-mark-putih.svg`),
/// unchanged except that it is decorative here: the words next to it say
/// "Sakala".
const MARK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="7.36 -1.50 75.29 111.22" fill="none" aria-hidden="true" focusable="false">
<path d="M59.9579 41.315L73.7477 29.9708C75.7741 28.3039 75.6737 25.1702 73.5448 23.6364L45.9499 3.75461C44.5849 2.77111 42.7504 2.74691 41.3598 3.69406L18.7912 19.0667C9.63907 25.3007 9.4343 38.723 18.392 45.2333L40.2842 61.1441" stroke="#FFFFFF" stroke-width="6" stroke-linecap="round"/>
<path d="M44.0316 68.1163C42.047 68.1163 40.4105 66.4896 40.4105 64.4493C40.4107 62.4092 42.0471 60.7833 44.0316 60.7833C46.0162 60.7833 47.6525 62.4092 47.6527 64.4493C47.6527 66.4897 46.0163 68.1163 44.0316 68.1163Z" stroke="#FFFFFF" stroke-width="4"/>
<path d="M33.2947 41.8663L23.8719 35.1245C21.6184 33.5122 21.6471 30.1526 23.9278 28.579L33.2947 22.1163" stroke="#FFFFFF" stroke-width="6" stroke-linecap="round"/>
<path d="M30.0421 66.9177L16.2523 78.2618C14.2259 79.9288 14.3263 83.0624 16.4552 84.5963L44.0501 104.478C45.4151 105.462 47.2496 105.486 48.6402 104.539L71.2088 89.166C80.3609 82.932 80.5657 69.5097 71.608 62.9994L49.7158 47.0886" stroke="#FFFFFF" stroke-width="6" stroke-linecap="round"/>
<path d="M45.9684 40.1163C47.953 40.1164 49.5895 41.743 49.5895 43.7833C49.5893 45.8235 47.9529 47.4493 45.9684 47.4493C43.9838 47.4493 42.3475 45.8235 42.3473 43.7833C42.3473 41.743 43.9837 40.1163 45.9684 40.1163Z" stroke="#FFFFFF" stroke-width="4"/>
<path d="M56.7053 66.3663L66.1281 73.1082C68.3816 74.7205 68.3529 78.0801 66.0722 79.6536L56.7053 86.1163" stroke="#FFFFFF" stroke-width="6" stroke-linecap="round"/>
</svg>"##;

/// The waking page's `<main>`: one heading, the waiting state, and the state
/// the script swaps in once the wait is over, with the heading's text from
/// `data-not-waking`. The `<noscript>` form posts the same wake
/// request into a hidden frame, so the page stays put and its slow refresh
/// opens the app once it is up.
const WAKING_MAIN: &str = r#"<main>
<div class="bar" id="bar" aria-hidden="true"></div>
<h1 id="heading" data-not-waking="SAKALA_NOT_WAKING_TITLE">SAKALA_WAKING_TITLE</h1>
<div id="waking">
<p class="lead">SAKALA_WAKING_TEXT</p>
<noscript>
<form method="post" action="SAKALA_WAKE_URL" target="sakala-wake">
<p class="hint">SAKALA_NO_SCRIPT_TEXT</p>
<button type="submit">SAKALA_NO_SCRIPT_LABEL</button>
</form>
<iframe name="sakala-wake" title="SAKALA_NO_SCRIPT_FRAME_TITLE" hidden></iframe>
</noscript>
<p class="hint">SAKALA_WAKING_OWNER</p>
</div>
<div id="not-waking" hidden>
<p class="lead">SAKALA_NOT_WAKING_TEXT</p>
<button type="button" id="retry">SAKALA_RETRY_LABEL</button>
<p class="hint">SAKALA_NOT_WAKING_OWNER</p>
</div>
</main>
"#;

/// The waking page's script. It keeps its count of reloads in
/// `sessionStorage`, under keys only it uses: it asks to wake the project on
/// the visit's first load and every `wakeEvery` reloads after it, reloads every
/// `reloadMs` up to `maxReloads` times, then swaps in the second state, whose
/// button starts one more round with a wake request of its own. Every `{` opens
/// a block whose text up to the next `}` holds a `:` or `;`, so none reads as a
/// Caddy placeholder.
const WAKING_SCRIPT: &str = r#"
(()=>{
const wakeUrl="SAKALA_WAKE_URL";
const reloadMs=SAKALA_RELOAD_MS,maxReloads=SAKALA_MAX_RELOADS,wakeEvery=SAKALA_WAKE_EVERY,slowReloadMs=SAKALA_SLOW_RELOAD_MS,visitGapMs=SAKALA_VISIT_GAP_MS;
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
if(reloads<maxReloads){
if(reloads%wakeEvery===0)wake();
reloadIn(reloadMs);
return;
}
const heading=document.getElementById("heading");
heading.textContent=heading.dataset.notWaking;
document.title=heading.textContent;
document.getElementById("bar").hidden=true;
document.getElementById("waking").hidden=true;
document.getElementById("not-waking").hidden=false;
document.getElementById("retry").addEventListener("click",()=>{
write(reloadsKey,"1");write(lastLoadKey,String(Date.now()));wake();location.reload();
});
})();
"#;

const UNAVAILABLE_MAIN: &str = r#"<main>
<h1>SAKALA_UNAVAILABLE_TITLE</h1>
<p class="lead">SAKALA_UNAVAILABLE_TEXT</p>
<p class="hint">SAKALA_UNAVAILABLE_OWNER</p>
</main>
"#;

/// A rendered page and the `Content-Security-Policy` that lets exactly its own
/// inline blocks run.
struct Page {
    html: String,
    policy: String,
}

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

/// The Console's name, linked when its URL is known.
fn console_reference(console_url: Option<&Url>) -> String {
    match console_url {
        Some(url) => format!("<a href=\"{}\">{CONSOLE_NAME}</a>", url.as_str()),
        None => CONSOLE_NAME.to_owned(),
    }
}

/// A CSP source for one inline block, as the browser hashes it: the exact text
/// between the tags.
fn inline_hash(block: &str) -> String {
    format!(
        "'sha256-{}'",
        BASE64.encode(Sha256::digest(block.as_bytes()))
    )
}

fn document(title: &str, head: &str, main: &str, script: Option<&str>) -> String {
    let script = script.map_or_else(String::new, |script| format!("<script>{script}</script>\n"));
    format!(
        "<!doctype html>\n\
         <html lang=\"id\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex\">\n\
         <meta name=\"theme-color\" content=\"#08413D\">\n\
         <title>{title}</title>\n\
         {head}<style>{STYLE}</style>\n\
         </head>\n\
         <body>\n\
         {main}<footer>\n\
         {MARK}\n\
         <span>{HOSTED_BY}</span>\n\
         </footer>\n\
         {script}</body>\n\
         </html>"
    )
}

fn waking_page(wake_url: &Url, console_url: Option<&Url>) -> Page {
    let console = console_reference(console_url);
    let wait_minutes = (MAX_WAKE_WAIT_SECONDS / SECONDS_PER_MINUTE).to_string();
    let main = fill(
        WAKING_MAIN,
        &[
            ("SAKALA_WAKING_TITLE", WAKING_TITLE),
            ("SAKALA_WAKING_TEXT", WAKING_TEXT),
            ("SAKALA_WAKE_ESTIMATE", WAKE_ESTIMATE),
            ("SAKALA_NO_SCRIPT_TEXT", NO_SCRIPT_TEXT),
            ("SAKALA_NO_SCRIPT_LABEL", NO_SCRIPT_LABEL),
            ("SAKALA_NO_SCRIPT_FRAME_TITLE", NO_SCRIPT_FRAME_TITLE),
            ("SAKALA_WAKING_OWNER", WAKING_OWNER),
            ("SAKALA_NOT_WAKING_TITLE", NOT_WAKING_TITLE),
            ("SAKALA_NOT_WAKING_TEXT", NOT_WAKING_TEXT),
            ("SAKALA_WAIT_MINUTES", &wait_minutes),
            ("SAKALA_RETRY_LABEL", RETRY_LABEL),
            ("SAKALA_NOT_WAKING_OWNER", NOT_WAKING_OWNER),
            ("SAKALA_CONSOLE", &console),
            ("SAKALA_WAKE_URL", wake_url.as_str()),
        ],
    );
    let script = fill(
        WAKING_SCRIPT,
        &[
            ("SAKALA_WAKE_URL", wake_url.as_str()),
            ("SAKALA_RELOAD_MS", &millis(RELOAD_INTERVAL_SECONDS)),
            ("SAKALA_MAX_RELOADS", &MAX_RELOADS.to_string()),
            ("SAKALA_WAKE_EVERY", &WAKE_REPEAT_RELOADS.to_string()),
            ("SAKALA_SLOW_RELOAD_MS", &millis(SLOW_RELOAD_SECONDS)),
            ("SAKALA_VISIT_GAP_MS", &millis(VISIT_GAP_SECONDS)),
        ],
    );
    let wake_origin = wake_url.origin().ascii_serialization();
    Page {
        policy: format!(
            "default-src 'none'; script-src {script}; style-src {style}; connect-src {wake_origin}; form-action {wake_origin}; frame-src {wake_origin}; base-uri 'none'",
            script = inline_hash(&script),
            style = inline_hash(STYLE),
        ),
        html: document(
            WAKING_TITLE,
            &format!(
                "<noscript><meta http-equiv=\"refresh\" content=\"{SLOW_RELOAD_SECONDS}\"></noscript>\n"
            ),
            &main,
            Some(&script),
        ),
    }
}

fn unavailable_page(console_url: Option<&Url>) -> Page {
    let console = console_reference(console_url);
    let main = fill(
        UNAVAILABLE_MAIN,
        &[
            ("SAKALA_UNAVAILABLE_TITLE", UNAVAILABLE_TITLE),
            ("SAKALA_UNAVAILABLE_TEXT", UNAVAILABLE_TEXT),
            ("SAKALA_UNAVAILABLE_OWNER", UNAVAILABLE_OWNER),
            ("SAKALA_CONSOLE", &console),
        ],
    );
    Page {
        policy: format!(
            "default-src 'none'; style-src {style}; form-action 'none'; base-uri 'none'",
            style = inline_hash(STYLE),
        ),
        html: document(UNAVAILABLE_TITLE, "", &main, None),
    }
}

/// The Caddyfile site block of a sleeping route: HTTP 503,
/// `Cache-Control: no-store`, `X-Robots-Tag: noindex`, the page's
/// `Content-Security-Policy`, and the page. Only the waking page sends
/// `Retry-After`.
pub(crate) fn site_block(route: &SleepingRouteSpec) -> Result<String, RuntimeError> {
    if !std::iter::once(&route.domain)
        .chain(&route.custom_domains)
        .all(|domain| domain_safe(domain))
    {
        return Err(RuntimeError::InvalidCommand(
            "a domain of a sleeping route holds characters that are not safe in a route".to_owned(),
        ));
    }
    let console_url = route.console_url.as_ref();
    if console_url.is_some_and(|url| !page_safe(url.as_str())) {
        return Err(RuntimeError::InvalidCommand(
            "the Console URL of a sleeping route holds characters that are not safe in the page"
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
                waking_page(wake_url, console_url),
                format!("\t\tRetry-After \"{RETRY_AFTER_SECONDS}\"\n"),
            )
        }
        SleepingRoutePage::Unavailable => (unavailable_page(console_url), String::new()),
    };
    let mut block = format!(
        "{addresses} {{\n\
         \theader {{\n\
         \t\tContent-Type \"text/html; charset=utf-8\"\n\
         \t\tCache-Control \"no-store\"\n\
         {retry_after}\
         \t\tX-Robots-Tag \"noindex\"\n\
         \t\tContent-Security-Policy \"{policy}\"\n\
         \t}}\n\
         \trespond <<{HEREDOC_MARKER}\n",
        addresses = super::site_addresses(&route.domain, &route.custom_domains),
        policy = page.policy,
    );
    for line in page.html.lines() {
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
        CONSOLE_NAME, HEREDOC_MARKER, HOSTED_BY, MAX_RELOADS, MAX_WAKE_WAIT_SECONDS,
        NO_SCRIPT_FRAME_TITLE, NO_SCRIPT_LABEL, NO_SCRIPT_TEXT, NOT_WAKING_OWNER, NOT_WAKING_TEXT,
        NOT_WAKING_TITLE, RELOAD_INTERVAL_SECONDS, RETRY_LABEL, SECONDS_PER_MINUTE,
        SleepingRoutePage, SleepingRouteSpec, UNAVAILABLE_OWNER, UNAVAILABLE_TEXT,
        UNAVAILABLE_TITLE, WAKE_ESTIMATE, WAKE_REPEAT_RELOADS, WAKE_REPEAT_SECONDS, WAKING_OWNER,
        WAKING_TEXT, WAKING_TITLE, check_public_api_url, check_public_console_url, inline_hash,
        site_block, unavailable_page, wake_url, waking_page,
    };

    const WAKE: &str = "https://api.sakala.dev/api/v1/public/projects/portfolio/wake";
    const CONSOLE: &str = "https://app.sakala.dev";
    /// `sakala.rate_limits.project_wake` in sakala-api: wake requests a minute
    /// per visitor address and project.
    const API_WAKE_LIMIT_PER_MINUTE: u32 = 6;

    fn console() -> Url {
        check_public_console_url(CONSOLE).expect("Console URL")
    }

    fn route(domain: &str, page: SleepingRoutePage) -> SleepingRouteSpec {
        SleepingRouteSpec {
            custom_domains: Vec::new(),
            project_id: Uuid::nil(),
            deployment_id: Uuid::nil(),
            domain: domain.to_owned(),
            page,
            console_url: Some(console()),
        }
    }

    fn waking(wake: &str) -> SleepingRoutePage {
        SleepingRoutePage::Waking {
            wake_url: Url::parse(wake).expect("URL"),
        }
    }

    /// Every page this module renders, with and without a Console URL.
    fn pages() -> Vec<String> {
        let wake = Url::parse(WAKE).expect("URL");
        let console = console();
        vec![
            waking_page(&wake, Some(&console)).html,
            waking_page(&wake, None).html,
            unavailable_page(Some(&console)).html,
            unavailable_page(None).html,
        ]
    }

    /// The text between `open` and the next `close`.
    fn between<'a>(text: &'a str, open: &str, close: &str) -> &'a str {
        let start = text.find(open).expect("opening tag") + open.len();
        let end = text[start..].find(close).expect("closing tag") + start;
        &text[start..end]
    }

    /// The page as a browser receives it: the heredoc lines without the
    /// indentation of the closing marker.
    fn served(block: &str) -> String {
        let body = between(
            block,
            "respond <<SAKALA_SLEEPING_PAGE\n",
            "\t\tSAKALA_SLEEPING_PAGE",
        );
        body.lines()
            .map(|line| line.strip_prefix("\t\t").expect("indented heredoc line"))
            .collect::<Vec<_>>()
            .join("\n")
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
    fn the_public_url_checks_keep_a_path_prefix_and_name_the_reason() {
        for check in [check_public_api_url, check_public_console_url] {
            assert_eq!(
                check("https://example.ac.id/sakala/")
                    .expect("prefix")
                    .as_str(),
                "https://example.ac.id/sakala"
            );
            assert_eq!(
                check("http://127.0.0.1:8100"),
                Ok(Url::parse("http://127.0.0.1:8100").expect("URL"))
            );
            assert_eq!(check("api.sakala.dev"), Err("is not a URL"));
            assert!(
                check("https://api.sakala.dev/it's")
                    .expect_err("quote")
                    .contains("not safe")
            );
            for refused in [
                "ftp://app.sakala.dev",
                "https://user:secret@app.sakala.dev",
                "https://app.sakala.dev/?a=1",
                "https://app.sakala.dev/#x",
                "javascript:alert(1)",
            ] {
                assert!(check(refused).is_err(), "{refused}");
            }
        }
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
            "<script>",
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
    fn the_waking_block_answers_503_with_retry_after_no_store_and_a_policy() {
        let block = site_block(&route("portfolio.sakala.app", waking(WAKE))).expect("block");

        assert!(
            block.starts_with(
                "portfolio.sakala.app:80 {\n\
                 \theader {\n\
                 \t\tContent-Type \"text/html; charset=utf-8\"\n\
                 \t\tCache-Control \"no-store\"\n\
                 \t\tRetry-After \"5\"\n\
                 \t\tX-Robots-Tag \"noindex\"\n\
                 \t\tContent-Security-Policy \"default-src 'none'; script-src 'sha256-"
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
    fn the_policy_allows_exactly_the_served_inline_blocks_and_the_wake_origin() {
        let block = site_block(&route("portfolio.sakala.app", waking(WAKE))).expect("block");
        let policy = between(&block, "Content-Security-Policy \"", "\"\n");
        let page = served(&block);

        assert_eq!(
            policy,
            format!(
                "default-src 'none'; script-src {}; style-src {}; connect-src https://api.sakala.dev; form-action https://api.sakala.dev; frame-src https://api.sakala.dev; base-uri 'none'",
                inline_hash(between(&page, "<script>", "</script>")),
                inline_hash(between(&page, "<style>", "</style>")),
            )
        );
        // One block of each, so the hashes cover everything that runs.
        assert_eq!(page.matches("<script").count(), 1);
        assert_eq!(page.matches("<style").count(), 1);
        assert!(
            !page.contains(" style="),
            "inline style attributes need their own hash"
        );
        assert!(
            !page.split(" on").skip(1).any(|rest| {
                rest.split('=').next().is_some_and(|name| {
                    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_lowercase())
                })
            }),
            "inline event handlers are refused by the policy"
        );

        let unavailable = site_block(&route(
            "portfolio.sakala.app",
            SleepingRoutePage::Unavailable,
        ))
        .expect("block");
        let page = served(&unavailable);
        assert_eq!(
            between(&unavailable, "Content-Security-Policy \"", "\"\n"),
            format!(
                "default-src 'none'; style-src {}; form-action 'none'; base-uri 'none'",
                inline_hash(between(&page, "<style>", "</style>")),
            )
        );
    }

    #[test]
    fn the_unavailable_block_answers_a_static_503_without_retry_after() {
        let block = site_block(&route(
            "portfolio.sakala.app",
            SleepingRoutePage::Unavailable,
        ))
        .expect("block");

        assert!(
            block.starts_with(
                "portfolio.sakala.app:80 {\n\
                 \theader {\n\
                 \t\tContent-Type \"text/html; charset=utf-8\"\n\
                 \t\tCache-Control \"no-store\"\n\
                 \t\tX-Robots-Tag \"noindex\"\n\
                 \t\tContent-Security-Policy \"default-src 'none'; style-src 'sha256-"
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
        assert!(block.contains(&format!("\t\t<p class=\"lead\">{UNAVAILABLE_TEXT}</p>\n")));
        for active in [
            "<script",
            "<noscript",
            "refresh",
            "fetch",
            "wake",
            "reload",
            "<button",
            "<form",
            "<iframe",
        ] {
            assert!(!block.contains(active), "{active} in {block}");
        }
    }

    #[test]
    fn the_waking_page_caps_its_reloads_and_asks_again_while_it_waits() {
        let page = waking_page(&Url::parse(WAKE).expect("URL"), None).html;

        assert_eq!(MAX_RELOADS, 40);
        assert_eq!(WAKE_REPEAT_RELOADS, 5);
        assert_eq!(
            WAKE_REPEAT_RELOADS * RELOAD_INTERVAL_SECONDS,
            WAKE_REPEAT_SECONDS
        );
        // One tab never asks more often than the control plane accepts: the
        // first load and one request every WAKE_REPEAT_SECONDS.
        const { assert!(SECONDS_PER_MINUTE / WAKE_REPEAT_SECONDS < API_WAKE_LIMIT_PER_MINUTE) };
        assert!(page.contains(
            "const reloadMs=3000,maxReloads=40,wakeEvery=5,slowReloadMs=15000,visitGapMs=60000;\n"
        ));
        // While it waits: on the visit's first load and every wakeEvery reloads.
        assert!(page.contains(
            "\nif(reloads<maxReloads){\nif(reloads%wakeEvery===0)wake();\nreloadIn(reloadMs);\nreturn;\n}\n"
        ));
        // Three callers: the counted wait, the uncounted slow loop, the button.
        assert_eq!(page.matches("wake()").count(), 3, "{page}");
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
            "<main>\n<div class=\"bar\" id=\"bar\" aria-hidden=\"true\"></div>\n<h1 id=\"heading\" data-not-waking=\"{NOT_WAKING_TITLE}\">{WAKING_TITLE}</h1>\n<div id=\"waking\">\n<p class=\"lead\">{}</p>\n",
            WAKING_TEXT.replace("SAKALA_WAKE_ESTIMATE", WAKE_ESTIMATE)
        )));
        assert!(page.contains(&format!(
            "<div id=\"not-waking\" hidden>\n<p class=\"lead\">Sudah ditunggu 2 menit, tetapi aplikasinya belum menyala.</p>\n<button type=\"button\" id=\"retry\">{RETRY_LABEL}</button>\n"
        )));
        // One heading; the script gives it the second state's text.
        assert_eq!(page.matches("<h1").count(), 1, "{page}");
        assert!(page.contains(
            "heading.textContent=heading.dataset.notWaking;\ndocument.title=heading.textContent;\n"
        ));
        for id in ["heading", "bar", "waking", "not-waking", "retry"] {
            assert!(page.contains(&format!("getElementById(\"{id}\")")), "{id}");
        }
    }

    #[test]
    fn without_javascript_the_page_offers_a_form_that_wakes_the_project() {
        let page = waking_page(&Url::parse(WAKE).expect("URL"), None).html;
        let fallback = between(&page, "<noscript>\n<form", "</noscript>");

        assert!(fallback.starts_with(&format!(
            " method=\"post\" action=\"{WAKE}\" target=\"sakala-wake\">\n<p class=\"hint\">{NO_SCRIPT_TEXT}</p>\n<button type=\"submit\">{NO_SCRIPT_LABEL}</button>\n</form>\n"
        )));
        assert!(fallback.contains(&format!(
            "<iframe name=\"sakala-wake\" title=\"{NO_SCRIPT_FRAME_TITLE}\" hidden></iframe>"
        )));
    }

    #[test]
    fn the_estimate_and_the_wait_are_the_ones_the_constants_name() {
        assert!(WAKING_TEXT.contains("Biasanya siap dalam SAKALA_WAKE_ESTIMATE,"));
        let page = waking_page(&Url::parse(WAKE).expect("URL"), None).html;
        assert!(page.contains(
            "Aplikasi ini tidur karena beberapa waktu tidak dikunjungi. Biasanya siap dalam kurang dari satu menit, dan halaman ini akan terbuka sendiri."
        ));
        assert_eq!(MAX_WAKE_WAIT_SECONDS, 120);
        assert!(NOT_WAKING_TEXT.contains("SAKALA_WAIT_MINUTES menit"));
    }

    #[test]
    fn every_page_names_its_host_and_links_the_owner_to_the_console_when_known() {
        let wake = Url::parse(WAKE).expect("URL");
        let console = console();
        let link = "<a href=\"https://app.sakala.dev/\">Console Sakala</a>";
        for (page, owner) in [
            (waking_page(&wake, Some(&console)).html, WAKING_OWNER),
            (unavailable_page(Some(&console)).html, UNAVAILABLE_OWNER),
        ] {
            assert!(
                page.contains(&format!(
                    "<p class=\"hint\">{}</p>",
                    owner.replace("SAKALA_CONSOLE", link)
                )),
                "{page}"
            );
            assert!(page.contains(&format!("<span>{HOSTED_BY}</span>")));
        }
        let waking = waking_page(&wake, Some(&console)).html;
        assert!(waking.contains(&NOT_WAKING_OWNER.replace("SAKALA_CONSOLE", link)));

        for page in [waking_page(&wake, None).html, unavailable_page(None).html] {
            assert!(!page.contains("<a "), "{page}");
            assert!(page.contains(&format!("di {CONSOLE_NAME}.</p>")), "{page}");
        }
    }

    #[test]
    fn no_page_loads_anything_or_repeats_the_visited_domain() {
        let block = site_block(&route("portfolio.sakala.app", waking(WAKE))).expect("block");
        let page = served(&block);
        // The only URLs are the wake endpoint, the Console link, and the SVG
        // namespace, which is a name and never fetched.
        let urls: Vec<&str> = page
            .match_indices("://")
            .map(|(index, _)| {
                let start = page[..index]
                    .rfind(['"', '\'', ' ', '>', '('])
                    .map_or(0, |quote| quote + 1);
                page[start..]
                    .split(['"', '\'', ' ', '<', ')'])
                    .next()
                    .unwrap_or_default()
            })
            .collect();
        for url in &urls {
            assert!(
                [
                    WAKE,
                    "https://app.sakala.dev/",
                    "http://www.w3.org/2000/svg"
                ]
                .contains(url),
                "unexpected URL {url} in {page}"
            );
        }
        for loads in ["src=", "<link", "@import", "url(", "<img"] {
            assert!(!page.contains(loads), "{loads}");
        }
        assert!(!page.contains("portfolio.sakala.app"), "{page}");
    }

    #[test]
    fn the_copy_is_plain_text() {
        for copy in [
            WAKE_ESTIMATE,
            WAKING_TITLE,
            WAKING_TEXT,
            NO_SCRIPT_TEXT,
            NO_SCRIPT_LABEL,
            NO_SCRIPT_FRAME_TITLE,
            WAKING_OWNER,
            NOT_WAKING_TITLE,
            NOT_WAKING_TEXT,
            NOT_WAKING_OWNER,
            RETRY_LABEL,
            UNAVAILABLE_TITLE,
            UNAVAILABLE_TEXT,
            UNAVAILABLE_OWNER,
            CONSOLE_NAME,
            HOSTED_BY,
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
            "Portfolio.sakala.app",
            "a.sakala.app {",
            "a\nb",
            "{env.X}",
            "<b>.sakala.app",
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
    fn refuses_a_wake_or_console_url_that_is_not_safe_in_the_page() {
        let error = site_block(&route(
            "portfolio.sakala.app",
            waking("https://api.sakala.dev/it's"),
        ))
        .expect_err("a quote cannot reach the script");
        assert!(error.to_string().contains("wake URL"), "{error}");

        for page in [waking(WAKE), SleepingRoutePage::Unavailable] {
            let mut unsafe_console = route("portfolio.sakala.app", page);
            unsafe_console.console_url =
                Some(Url::parse("https://app.sakala.dev/it's").expect("URL"));
            let error = site_block(&unsafe_console).expect_err("a quote cannot reach the link");
            assert!(error.to_string().contains("Console URL"), "{error}");
        }
    }

    #[test]
    fn custom_domains_show_the_same_page_and_wake_by_the_project_slug() {
        let wake = SleepingRoutePage::Waking {
            wake_url: wake_url("https://api.sakala.dev", "portfolio").expect("wake URL"),
        };
        let route = SleepingRouteSpec {
            custom_domains: vec!["www.contoh.com".to_owned()],
            ..route("portfolio.sakala.app", wake)
        };
        let block = site_block(&route).expect("site block");

        assert!(
            block.starts_with("portfolio.sakala.app:80, www.contoh.com:80 {\n"),
            "{block}"
        );
        assert!(block.contains("/api/v1/public/projects/portfolio/wake"));

        let unsafe_custom = SleepingRouteSpec {
            custom_domains: vec!["www.contoh.com {".to_owned()],
            ..route
        };
        assert!(site_block(&unsafe_custom).is_err());
    }
}
