//! Recognises a framework that refuses requests for the project's domain.
//!
//! Vite's dev and preview servers answer 403 to any `Host` outside `allowedHosts`,
//! and Django answers 400 outside `ALLOWED_HOSTS`. A 4xx counts as "the app
//! answers", so such a deployment used to succeed while every visitor got the
//! refusal. The readiness probe cannot tell this from a login page by its status
//! alone, so on 400 or 403 the Agent asks once more with the domain visitors use
//! and reads the start of the reply.

use std::fmt;

use crate::CommandSpec;

/// Statuses that the frameworks below use for a refused host. Only these pay for
/// the second request.
pub(crate) const HOST_CHECK_STATUSES: [u16; 2] = [400, 403];
/// Enough for the status line, headers, and the start of any of the bodies below.
pub(crate) const MAX_REPLY_BYTES: usize = 4096;
const VITE_STATUS: u16 = 403;
const DJANGO_STATUS: u16 = 400;
/// Vite 6 and later (`packages/vite/src/node/server/middlewares/hostCheck.ts`),
/// verified against Vite 8.3.5:
/// ``Blocked request. This host ("…") is not allowed.\nTo allow this host, add "…"
/// to `preview.allowedHosts` in vite.config.js.``
const VITE_BLOCKED: &str = "Blocked request. This host (";
const VITE_NOT_ALLOWED: &str = "is not allowed";
const VITE_PREVIEW_SETTING: &str = "preview.allowedHosts";
const VITE_SERVER_SETTING: &str = "server.allowedHosts";
/// Django's `DisallowedHost` (`django/http/request.py`), which only shows on the
/// debug error page: with `DEBUG = False` the reply is a bare "Bad Request (400)"
/// that names no cause, so it is not recognised.
const DJANGO_INVALID_HOST: &str = "Invalid HTTP_HOST header";
const DJANGO_ALLOWED_HOSTS: &str = "ALLOWED_HOSTS";
/// Sends one request with the visitors' `Host` and prints at most
/// `MAX_REPLY_BYTES` of the reply. BusyBox `wget` prints no body for a 4xx, so
/// this uses BusyBox `nc` from the same router image. The values arrive as
/// positional parameters, never as part of the script.
const REQUEST_SCRIPT: &str = "printf 'GET / HTTP/1.1\\r\\nHost: %s\\r\\nUser-Agent: sakala-agent-readiness\\r\\nAccept: text/html,*/*\\r\\nConnection: close\\r\\n\\r\\n' \"$1\" | nc -w \"$4\" \"$2\" \"$3\" | head -c \"$5\"";

/// A framework setting that refuses the project's domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostCheck {
    VitePreview,
    ViteDevServer,
    Django,
}

impl HostCheck {
    /// What the project has to change, for the failure message.
    pub(crate) fn describe(self, host: &str) -> String {
        match self {
            Self::VitePreview => format!(
                "Vite's preview server refuses requests for {host}, so every visitor would get HTTP 403; add it to preview.allowedHosts in vite.config (for example preview: {{ allowedHosts: ['{host}'] }}), or clear the start command so Sakala serves the build output as a static site"
            ),
            Self::ViteDevServer => format!(
                "the start command runs Vite's development server, which refuses requests for {host}, so every visitor would get HTTP 403; clear the start command so Sakala serves the build output as a static site, or add the host to server.allowedHosts in vite.config"
            ),
            Self::Django => format!(
                "Django refuses requests for {host} (DisallowedHost), so every visitor would get HTTP 400; add it to ALLOWED_HOSTS in settings.py, and turn DEBUG off for production"
            ),
        }
    }
}

impl fmt::Display for HostCheck {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::VitePreview => "vite-preview",
            Self::ViteDevServer => "vite-dev-server",
            Self::Django => "django",
        })
    }
}

/// The request with the visitors' `Host`, run in the router container like the
/// readiness probe.
pub(crate) fn request_command(
    router_container: &str,
    container: &str,
    port: u16,
    host: &str,
    read_seconds: u64,
) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("exec")
        .arg(router_container)
        .arg("sh")
        .arg("-c")
        .arg(REQUEST_SCRIPT)
        .arg("sh")
        .arg(host)
        .arg(container)
        .arg(port.to_string())
        .arg(read_seconds.to_string())
        .arg(MAX_REPLY_BYTES.to_string())
}

/// Which host check refused the request, judged from the raw reply. Anything
/// else, including a reply that cannot be read, is not a refusal, so a deployment
/// only fails here on a reply that says exactly this. The app wrote the reply, so
/// nothing from it reaches a message.
pub(crate) fn refused_by(reply: &str) -> Option<HostCheck> {
    let reply = &reply[..floor_char_boundary(reply, MAX_REPLY_BYTES)];
    let status = status_of(reply)?;
    if status == VITE_STATUS && reply.contains(VITE_BLOCKED) && reply.contains(VITE_NOT_ALLOWED) {
        if reply.contains(VITE_PREVIEW_SETTING) {
            return Some(HostCheck::VitePreview);
        }
        if reply.contains(VITE_SERVER_SETTING) {
            return Some(HostCheck::ViteDevServer);
        }
        // An older message without the setting: `vite preview` is what a start
        // command usually runs.
        return Some(HostCheck::VitePreview);
    }
    if status == DJANGO_STATUS
        && reply.contains(DJANGO_INVALID_HOST)
        && reply.contains(DJANGO_ALLOWED_HOSTS)
    {
        return Some(HostCheck::Django);
    }
    None
}

/// The status of an `HTTP/1.x 403 Forbidden` status line.
fn status_of(reply: &str) -> Option<u16> {
    let mut words = reply.lines().next()?.split_whitespace();
    words.next()?.strip_prefix("HTTP/")?;
    words.next()?.parse().ok()
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    (0..=index)
        .rev()
        .find(|position| text.is_char_boundary(*position))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Vite 8.3.5 `vite preview` sent for a host outside `allowedHosts`,
    /// captured with exactly this request through `nc` on 2026-11-19.
    const VITE_PREVIEW_REPLY: &str = "HTTP/1.1 403 Forbidden\r\nVary: Origin\r\nContent-Type: text/plain\r\nDate: Thu, 19 Nov 2026 04:48:51 GMT\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n9d\r\nBlocked request. This host (\"facility.sakala.app\") is not allowed.\nTo allow this host, add \"facility.sakala.app\" to `preview.allowedHosts` in vite.config.js.\r\n0\r\n\r\n";

    #[test]
    fn vite_preview_and_dev_server_refusals_are_told_apart() {
        assert_eq!(refused_by(VITE_PREVIEW_REPLY), Some(HostCheck::VitePreview));
        assert_eq!(
            refused_by(&VITE_PREVIEW_REPLY.replace("preview.allowedHosts", "server.allowedHosts")),
            Some(HostCheck::ViteDevServer)
        );
    }

    #[test]
    fn django_is_recognised_from_its_debug_page_only() {
        let debug_page = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<title>DisallowedHost at /</title>\n<pre class=\"exception_value\">Invalid HTTP_HOST header: &#x27;shop.sakala.app&#x27;. You may need to add &#x27;shop.sakala.app&#x27; to ALLOWED_HOSTS.</pre>";
        assert_eq!(refused_by(debug_page), Some(HostCheck::Django));
        let production_page =
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h1>Bad Request (400)</h1>";
        assert_eq!(refused_by(production_page), None);
    }

    #[test]
    fn other_refusals_and_unreadable_replies_are_not_host_checks() {
        for reply in [
            // A login wall or an API key check is a working app.
            "HTTP/1.1 403 Forbidden\r\n\r\nYou must sign in",
            // The right words with the wrong status.
            &VITE_PREVIEW_REPLY.replace("403 Forbidden", "200 OK"),
            // The Vite words in a 400, or the Django words in a 403.
            &VITE_PREVIEW_REPLY.replace("403 Forbidden", "400 Bad Request"),
            "HTTP/1.1 403 Forbidden\r\n\r\nInvalid HTTP_HOST header. ALLOWED_HOSTS",
            // Not HTTP at all, as when nc is missing from the router image.
            "sh: nc: not found",
            "",
        ] {
            assert_eq!(refused_by(reply), None, "{reply:?}");
        }
    }

    #[test]
    fn words_beyond_the_read_limit_do_not_count() {
        let reply = format!(
            "HTTP/1.1 403 Forbidden\r\n\r\n{}Blocked request. This host (\"x\") is not allowed.",
            "é".repeat(MAX_REPLY_BYTES)
        );
        assert_eq!(refused_by(&reply), None);
    }

    #[test]
    fn the_message_names_the_host_and_the_fix() {
        let message = HostCheck::VitePreview.describe("facility.sakala.app");
        assert!(
            message.contains("preview: { allowedHosts: ['facility.sakala.app'] }"),
            "{message}"
        );
        assert!(message.contains("clear the start command"), "{message}");
        assert!(
            HostCheck::Django
                .describe("shop.sakala.app")
                .contains("ALLOWED_HOSTS")
        );
    }

    #[test]
    fn the_host_reaches_the_script_only_as_a_parameter() {
        let command = request_command("sakala-caddy", "sakala-app-1", 3000, "a.sakala.app", 5);
        let arguments = command
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments[..5],
            ["exec", "sakala-caddy", "sh", "-c", REQUEST_SCRIPT]
        );
        assert_eq!(
            arguments[5..],
            ["sh", "a.sakala.app", "sakala-app-1", "3000", "5", "4096"]
        );
        assert!(!REQUEST_SCRIPT.contains("a.sakala.app"));
    }
}
