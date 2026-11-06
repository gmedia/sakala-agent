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
const UNPROCESSABLE_LINE: &str = "[REDACTED: this line could not be processed]";
/// Shorter values are likely to match ordinary log text, like `true` or `3000`,
/// and a secret that short gains little from being hidden.
const MIN_REDACTED_VALUE_CHARS: usize = 8;
/// Common configuration words that would otherwise erase ordinary log text.
const ORDINARY_VALUES: [&str; 5] = [
    "production",
    "development",
    "localhost",
    "0.0.0.0/0",
    "127.0.0.1",
];

/// Redacts the environment values of one command on top of [`redact_line`],
/// which only recognises key names and token prefixes.
///
/// Matching is exact, so an encoded form of a value (URL or base64) is not found.
#[derive(Clone, Default)]
pub struct ValueRedactor {
    values: Vec<String>,
}

impl ValueRedactor {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values = values
            .into_iter()
            .filter(|value| {
                value.chars().count() >= MIN_REDACTED_VALUE_CHARS
                    && !ORDINARY_VALUES.contains(&value.as_str())
            })
            .collect::<Vec<_>>();
        // Longest first, so a value containing another value is hidden whole.
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self { values }
    }

    /// Never panics: a line it cannot process is withheld. A panic here would kill
    /// the command's task before its cleanup and terminal report, and letting the
    /// line through could leak what it holds.
    #[must_use]
    pub fn redact(&self, line: &str) -> String {
        std::panic::catch_unwind(|| {
            let mut result = line.to_owned();
            for value in &self.values {
                if result.contains(value.as_str()) {
                    result = result.replace(value.as_str(), REDACTED);
                }
            }
            redact_line(&result)
        })
        .unwrap_or_else(|_| UNPROCESSABLE_LINE.to_owned())
    }
}

impl std::fmt::Debug for ValueRedactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValueRedactor")
            .field("values", &self.values.len())
            .finish()
    }
}

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
            let token_start = skip_whitespace(value, cursor + "bearer ".len());
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
        let token_start = skip_whitespace(value, start + "bearer ".len());
        let token_end = value.as_bytes()[token_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'"'))
            .map_or(value.len(), |relative| token_start + relative);
        // The offset only moves past a replacement that really made the text longer.
        search_from = if token_end > token_start {
            value.replace_range(token_start..token_end, REDACTED);
            token_start + REDACTED.len()
        } else {
            token_start
        };
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

/// More than one space after `bearer` must not hide the token behind it.
fn skip_whitespace(value: &str, from: usize) -> usize {
    value.as_bytes()[from..]
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map_or(value.len(), |relative| from + relative)
}

fn find_ascii_case_insensitive(value: &str, needle: &str, from: usize) -> Option<usize> {
    value
        .as_bytes()
        .get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        .map(|relative| from + relative)
}

#[cfg(test)]
mod tests {
    use super::{ValueRedactor, redact_line};

    const FRAGMENTS: [&str; 18] = [
        "bearer ",
        "Bearer",
        " ",
        "  ",
        ",",
        ";",
        "\"",
        "'",
        "=",
        ":",
        "x",
        "é",
        "token",
        "authorization",
        "ghp_",
        "password",
        "[REDACTED]",
        "\t",
    ];

    #[test]
    fn bearer_followed_by_a_delimiter_does_not_panic() {
        for line in [
            "bearer  x",
            "bearer ,x",
            "bearer ;",
            "bearer \"",
            "bearer ",
            "Bearer  ",
        ] {
            assert!(redact_line(line).starts_with(&line[..6]), "{line:?}");
        }
        assert_eq!(redact_line("bearer  secret"), "bearer  [REDACTED]");
    }

    /// Every sequence of up to four fragments must return, without a panic, and a
    /// repeated pass must not panic on the redactor's own output either.
    #[test]
    fn odd_inputs_never_panic_exhaustively() {
        let mut stack = vec![String::new()];
        while let Some(line) = stack.pop() {
            let once = redact_line(&line);
            let _ = redact_line(&once);
            if line.len() < 64 && count_parts(&line) < 4 {
                for fragment in FRAGMENTS {
                    stack.push(format!("{line}\u{1}{fragment}"));
                }
            }
        }
    }

    fn count_parts(line: &str) -> usize {
        line.matches('\u{1}').count()
    }

    /// Longer lines than the exhaustive pass, from a fixed seed so a failure repeats.
    #[test]
    fn odd_inputs_never_panic_on_long_lines() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..20_000 {
            let mut line = String::new();
            for _ in 0..(state % 40) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                line.push_str(FRAGMENTS[(state % FRAGMENTS.len() as u64) as usize]);
            }
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let _ = redact_line(&line);
        }
    }

    #[test]
    fn redaction_does_not_leave_a_token_behind_a_double_space() {
        assert!(!redact_line("Authorization: bearer  sekret").contains("sekret"));
    }

    fn redactor(values: &[&str]) -> ValueRedactor {
        ValueRedactor::new(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn redacts_a_value_under_any_name_or_none() {
        let redactor = redactor(&["kL9-unusual-value"]);

        assert_eq!(
            redactor.redact("connecting with kL9-unusual-value now, MY_THING=kL9-unusual-value"),
            "connecting with [REDACTED] now, MY_THING=[REDACTED]"
        );
    }

    #[test]
    fn short_and_ordinary_values_do_not_mangle_logs() {
        let redactor = redactor(&["true", "3000", "1234567", "production", "localhost"]);
        let line = "listening on localhost:3000 in production, debug=true, id=1234567";

        assert_eq!(redactor.redact(line), line);
    }

    #[test]
    fn a_value_is_redacted_from_its_minimum_length() {
        assert_eq!(
            redactor(&["12345678"]).redact("id 12345678"),
            "id [REDACTED]"
        );
    }

    #[test]
    fn a_value_containing_another_value_is_hidden_whole() {
        let redactor = redactor(&["short-secret", "short-secret-with-more"]);

        assert_eq!(
            redactor.redact("x short-secret-with-more y"),
            "x [REDACTED] y"
        );
    }

    #[test]
    fn key_name_redaction_still_applies() {
        assert_eq!(
            redactor(&["unrelated-value"]).redact("TOKEN=abc"),
            "TOKEN=[REDACTED]"
        );
    }

    #[test]
    fn debug_output_never_shows_a_value() {
        let debug = format!("{:?}", redactor(&["kL9-unusual-value"]));

        assert!(!debug.contains("kL9"), "{debug}");
    }

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
