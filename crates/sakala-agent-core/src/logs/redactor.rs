const SENSITIVE_KEYS: [&str; 5] = [
    "TOKEN=",
    "PASSWORD=",
    "SECRET=",
    "APP_KEY=",
    "DATABASE_URL=",
];
const REDACTED: &str = "[REDACTED]";

#[must_use]
pub fn redact_line(line: &str) -> String {
    let mut result = line.to_owned();

    for key in SENSITIVE_KEYS {
        let mut offset = 0;
        while let Some(relative_start) = result[offset..].find(key) {
            let value_start = offset + relative_start + key.len();
            let value_length = result[value_start..]
                .find(char::is_whitespace)
                .unwrap_or(result.len() - value_start);
            let value_end = value_start + value_length;

            result.replace_range(value_start..value_end, REDACTED);
            offset = value_start + REDACTED.len();
        }
    }

    result
}
