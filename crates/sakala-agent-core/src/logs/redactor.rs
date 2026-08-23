const SENSITIVE_KEYS: [&str; 10] = [
    "token",
    "password",
    "secret",
    "app_key",
    "database_url",
    "authorization",
    "api_key",
    "access_token",
    "refresh_token",
    "client_secret",
];
const TOKEN_PREFIXES: [&str; 4] = ["ghp_", "gho_", "ghs_", "github_pat_"];
const REDACTED: &str = "[REDACTED]";

#[must_use]
pub fn redact_line(line: &str) -> String {
    let mut result = line.to_owned();
    for key in SENSITIVE_KEYS {
        redact_assignment(&mut result, key);
    }
    redact_bearer_tokens(&mut result);
    redact_prefixed_tokens(&mut result);
    result
}

fn redact_assignment(value: &mut String, key: &str) {
    let mut search_from = 0;
    while let Some(start) = find_ascii_case_insensitive(value, key, search_from) {
        if start > 0 {
            let previous = value.as_bytes()[start - 1];
            if previous.is_ascii_alphanumeric() || previous == b'_' {
                search_from = start + key.len();
                continue;
            }
        }

        let mut cursor = start + key.len();
        while let Some(byte) = value.as_bytes().get(cursor) {
            if byte.is_ascii_whitespace() || *byte == b'\'' || *byte == b'"' {
                cursor += 1;
            } else {
                break;
            }
        }
        if !matches!(value.as_bytes().get(cursor), Some(b'=') | Some(b':')) {
            search_from = start + key.len();
            continue;
        }

        cursor += 1;
        while value
            .as_bytes()
            .get(cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            cursor += 1;
        }
        let quote = value
            .as_bytes()
            .get(cursor)
            .copied()
            .filter(|byte| matches!(byte, b'\'' | b'"'));
        if quote.is_some() {
            cursor += 1;
        }
        let end = if key == "authorization"
            && value.as_bytes()[cursor..]
                .get(.."bearer ".len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"bearer "))
        {
            let token_start = cursor + "bearer ".len();
            value.as_bytes()[token_start..]
                .iter()
                .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'"'))
                .map_or(value.len(), |relative| token_start + relative)
        } else {
            value.as_bytes()[cursor..]
                .iter()
                .position(|byte| match quote {
                    Some(quote) => *byte == quote,
                    None => byte.is_ascii_whitespace() || matches!(byte, b',' | b';'),
                })
                .map_or(value.len(), |relative| cursor + relative)
        };

        if end > cursor {
            value.replace_range(cursor..end, REDACTED);
            search_from = cursor + REDACTED.len();
        } else {
            search_from = cursor;
        }
    }
}

fn redact_bearer_tokens(value: &mut String) {
    let mut search_from = 0;
    while let Some(start) = find_ascii_case_insensitive(value, "bearer ", search_from) {
        let token_start = start + "bearer ".len();
        let token_end = value.as_bytes()[token_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'"'))
            .map_or(value.len(), |relative| token_start + relative);
        if token_end > token_start {
            value.replace_range(token_start..token_end, REDACTED);
        }
        search_from = token_start + REDACTED.len();
    }
}

fn redact_prefixed_tokens(value: &mut String) {
    for prefix in TOKEN_PREFIXES {
        let mut search_from = 0;
        while let Some(start) = find_ascii_case_insensitive(value, prefix, search_from) {
            let end = value.as_bytes()[start..]
                .iter()
                .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'"'))
                .map_or(value.len(), |relative| start + relative);
            value.replace_range(start..end, REDACTED);
            search_from = start + REDACTED.len();
        }
    }
}

fn find_ascii_case_insensitive(value: &str, needle: &str, from: usize) -> Option<usize> {
    value.as_bytes()[from..]
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        .map(|relative| from + relative)
}

#[cfg(test)]
mod tests {
    use super::redact_line;

    #[test]
    fn redacts_case_insensitive_json_and_authorization_values() {
        let line = r#"{"access_token":"abc","Authorization":"Bearer xyz"} password: open"#;
        assert_eq!(
            redact_line(line),
            r#"{"access_token":"[REDACTED]","Authorization":"[REDACTED]"} password: [REDACTED]"#
        );
    }

    #[test]
    fn redacts_github_token_without_a_key() {
        assert_eq!(
            redact_line("remote rejected github_pat_example123"),
            "remote rejected [REDACTED]"
        );
    }

    #[test]
    fn redacts_github_app_installation_tokens() {
        assert_eq!(
            redact_line("fetch failed with ghs_installation_token"),
            "fetch failed with [REDACTED]"
        );
    }
}
