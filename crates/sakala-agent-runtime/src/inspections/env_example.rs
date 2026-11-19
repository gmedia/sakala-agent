//! Reads the variables an environment example file (`.env.example`) lists, so the
//! Console can suggest them before the first deployment.
//!
//! The parser is written here on purpose. `dotenvy` 0.15.7, the maintained dotenv
//! crate, expands `$VAR` and `${VAR}` and its `apply_substitution` reads
//! `std::env::var` before the file's own values, with no way to turn that off. A line
//! such as `X=${SAKALA_AGENT_TOKEN}` in a repository would then copy the Agent's own
//! environment into the inspection result. `dotenv-parser` 0.1.3 (last release
//! 2021) rejects the whole file on one line it cannot parse and has no backtick
//! quotes, and `dotenv-verbatim` keeps quotes and inline comments and is too young to
//! pass the dependency health bar. The rules below are the inspection contract, not
//! a loader: values are only displayed, never exported, and nothing is expanded or
//! executed.
//!
//! Only the three candidate names are ever opened; `.env` and every other name never
//! are. The file is repository content, so it is read with the bounded, inside-the-
//! directory rules of [`repository_file`] and never logged.

use std::{collections::HashMap, path::Path};

use sakala_agent_protocol::{EnvExample, EnvExampleEntry, EnvExampleFile};
use tokio::fs;
use url::Url;

use crate::repository_file;

/// An example file is a short list of names; larger files are not read at all.
const MAX_ENV_EXAMPLE_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 200;
const MAX_KEY_CHARS: usize = 128;
const MAX_VALUE_CHARS: usize = 200;
const EXPORT_PREFIX: &str = "export";
const COMMENT: char = '#';
const ASSIGNMENT: char = '=';
const DOUBLE_QUOTE: char = '"';
const QUOTES: [char; 3] = [DOUBLE_QUOTE, '\'', '`'];
const ESCAPE: char = '\\';
const UTF8_BOM: char = '\u{feff}';
/// A PEM block (private key, certificate) in a value.
const PEM_MARKER: &str = "-----BEGIN";
const URL_SCHEME_SEPARATOR: &str = "://";

/// Keys whose value the database hint reads to pick the engine. The value is kept
/// even when the reported `example_value` is hidden, because a scheme such as
/// `postgres://` is not secret while the rest of a connection URL can be.
pub(super) const ENGINE_KEYS: [&str; 3] = ["DB_CONNECTION", "DATABASE_URL", "DB_URL"];

/// The reported example plus what the database hint may read from it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EnvExampleScan {
    pub(super) example: EnvExample,
    engine_values: HashMap<&'static str, String>,
}

impl EnvExampleScan {
    /// The value of one of [`ENGINE_KEYS`], unquoted, never reported as is.
    pub(super) fn engine_value(&self, key: &str) -> Option<&str> {
        self.engine_values.get(key).map(String::as_str)
    }
}

/// The first candidate file that exists in `context`, `None` when there is none or
/// it cannot be read safely: a symlink leading outside `context`, not a regular
/// file, or larger than [`MAX_ENV_EXAMPLE_BYTES`]. A later candidate is never read
/// instead of an unreadable earlier one.
pub(super) async fn read(context: &Path) -> Option<EnvExampleScan> {
    let root = fs::canonicalize(context).await.ok()?;
    for file in EnvExampleFile::CANDIDATES {
        let path = root.join(file.as_str());
        // Only the presence of the name decides which candidate wins.
        if fs::symlink_metadata(&path).await.is_err() {
            continue;
        }
        let contents = repository_file::read_bytes(&root, &path, MAX_ENV_EXAMPLE_BYTES).await?;
        return Some(parse(file, &String::from_utf8_lossy(&contents)));
    }
    None
}

/// What the text after `=` turned out to be.
enum RawValue<'a> {
    Value(String),
    /// A quote that does not close on its line; the value spans lines.
    Unterminated(char),
    Unquoted(&'a str),
}

pub(super) fn parse(file: EnvExampleFile, contents: &str) -> EnvExampleScan {
    let mut entries: Vec<EnvExampleEntry> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut engine_values = HashMap::new();
    let mut truncated = false;
    let mut open_quote: Option<char> = None;

    for line in contents.trim_start_matches(UTF8_BOM).lines() {
        if let Some(quote) = open_quote {
            // Lines of a multi-line value are skipped up to its closing quote.
            if closing_quote(line, quote).is_some() {
                open_quote = None;
            }
            continue;
        }
        let Some((key, raw)) = assignment(line) else {
            continue;
        };
        let value = match raw_value(raw) {
            RawValue::Value(value) => Some(value),
            RawValue::Unquoted(value) => Some(value.to_owned()),
            RawValue::Unterminated(quote) => {
                open_quote = Some(quote);
                None
            }
        }
        .filter(|value| !value.is_empty());
        if !is_valid_key(key) {
            continue;
        }

        // Last value wins, at the position of the first occurrence, like dotenv.
        let position = match positions.get(key) {
            Some(position) => *position,
            None if entries.len() < MAX_ENTRIES => {
                positions.insert(key.to_owned(), entries.len());
                entries.push(EnvExampleEntry {
                    key: key.to_owned(),
                    example_value: None,
                });
                entries.len() - 1
            }
            None => {
                truncated = true;
                continue;
            }
        };
        if let Some(engine_key) = ENGINE_KEYS.iter().find(|candidate| **candidate == key) {
            match &value {
                Some(value) => engine_values.insert(*engine_key, value.clone()),
                None => engine_values.remove(engine_key),
            };
        }
        entries[position].example_value = value
            .filter(|value| !looks_like_credential(value))
            .map(|value| truncate_chars(value, MAX_VALUE_CHARS));
    }

    EnvExampleScan {
        example: EnvExample {
            file,
            entries,
            truncated,
        },
        engine_values,
    }
}

/// `KEY` and the text after `=`, for a line that assigns something.
fn assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with(COMMENT) {
        return None;
    }
    let line = match line.strip_prefix(EXPORT_PREFIX) {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => line,
    };
    let (key, value) = line.split_once(ASSIGNMENT)?;
    Some((key.trim(), value))
}

fn raw_value(raw: &str) -> RawValue<'_> {
    let trimmed = raw.trim_start();
    if let Some(quote) = trimmed.chars().next().filter(|c| QUOTES.contains(c)) {
        let inner = &trimmed[quote.len_utf8()..];
        return match closing_quote(inner, quote) {
            // Anything after the closing quote, such as a comment, is ignored.
            Some(end) => RawValue::Value(unescape(&inner[..end], quote)),
            None => RawValue::Unterminated(quote),
        };
    }
    RawValue::Unquoted(without_inline_comment(raw).trim())
}

/// The byte offset of the quote that closes a value. Inside double quotes `\"` does
/// not close it; no other escape means anything.
fn closing_quote(text: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if quote == DOUBLE_QUOTE && character == ESCAPE {
            escaped = true;
        } else if character == quote {
            return Some(offset);
        }
    }
    None
}

/// Only an escaped closing quote is unescaped; `\n` and the rest stay literal text.
fn unescape(value: &str, quote: char) -> String {
    if quote == DOUBLE_QUOTE {
        value.replace("\\\"", "\"")
    } else {
        value.to_owned()
    }
}

/// An unquoted value up to a `#` that follows whitespace.
fn without_inline_comment(raw: &str) -> &str {
    let mut after_whitespace = false;
    for (offset, character) in raw.char_indices() {
        if character == COMMENT && after_whitespace {
            return &raw[..offset];
        }
        after_whitespace = character.is_whitespace();
    }
    raw
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,127}$`.
fn is_valid_key(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && key.len() <= MAX_KEY_CHARS
}

/// A PEM block or a URL whose userinfo carries a password.
fn looks_like_credential(value: &str) -> bool {
    if value.contains(PEM_MARKER) {
        return true;
    }
    match Url::parse(value) {
        Ok(url) => url.password().is_some(),
        // Not a URL the parser accepts: still treat `scheme://user:secret@host` as one.
        Err(_) => value
            .split_once(URL_SCHEME_SEPARATOR)
            .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
            .and_then(|authority| authority.rsplit_once('@'))
            .is_some_and(|(userinfo, _)| userinfo.contains(':')),
    }
}

/// At most `max` Unicode scalar values, cut on a character boundary.
fn truncate_chars(value: String, max: usize) -> String {
    match value.char_indices().nth(max) {
        Some((offset, _)) => value[..offset].to_owned(),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn entries(contents: &str) -> Vec<(String, Option<String>)> {
        parse(EnvExampleFile::Example, contents)
            .example
            .entries
            .into_iter()
            .map(|entry| (entry.key, entry.example_value))
            .collect()
    }

    fn pair(key: &str, value: Option<&str>) -> (String, Option<String>) {
        (key.to_owned(), value.map(str::to_owned))
    }

    #[test]
    fn export_prefix_whitespace_and_comments() {
        assert_eq!(
            entries(
                "# heading\n\n  export   APP_ENV = production  \nexport=1\nLOG_LEVEL=debug # inline\nHASH=a#b\nEMPTY_WITH_COMMENT= # nothing\n\tTABBED\t=\tyes\n"
            ),
            [
                pair("APP_ENV", Some("production")),
                pair("export", Some("1")),
                pair("LOG_LEVEL", Some("debug")),
                pair("HASH", Some("a#b")),
                pair("EMPTY_WITH_COMMENT", None),
                pair("TABBED", Some("yes")),
            ]
        );
    }

    #[test]
    fn quotes_are_removed_and_only_an_escaped_double_quote_is_unescaped() {
        assert_eq!(
            entries(concat!(
                "DOUBLE=\"Buku tamu # bukan komentar\" # komentar\n",
                "SINGLE='it is $HOME'\n",
                "BACKTICK=`echo hi`\n",
                "NEWLINE=\"line\\nnext\"\n",
                "ESCAPED=\"say \\\"hi\\\"\"\n",
                "EMPTY_QUOTED=\"\"\n",
            )),
            [
                pair("DOUBLE", Some("Buku tamu # bukan komentar")),
                pair("SINGLE", Some("it is $HOME")),
                pair("BACKTICK", Some("echo hi")),
                pair("NEWLINE", Some("line\\nnext")),
                pair("ESCAPED", Some("say \"hi\"")),
                pair("EMPTY_QUOTED", None),
            ]
        );
    }

    #[test]
    fn an_empty_value_is_null_and_a_line_without_equals_is_ignored() {
        assert_eq!(
            entries("APP_KEY=\nJUST_A_WORD\nAPP_URL=   \n"),
            [pair("APP_KEY", None), pair("APP_URL", None)]
        );
    }

    #[test]
    fn a_duplicate_key_keeps_its_first_position_and_its_last_value() {
        assert_eq!(
            entries("A=1\nB=2\nA=3\n"),
            [pair("A", Some("3")), pair("B", Some("2"))]
        );
    }

    #[test]
    fn an_unterminated_quote_is_null_and_its_following_lines_are_skipped() {
        assert_eq!(
            entries(concat!(
                "PRIVATE_KEY=\"-----BEGIN KEY\n",
                "HIDDEN=inside the value\n",
                "end of value\"\n",
                "AFTER=1\n",
                "SINGLE='never closed\n",
                "ALSO_HIDDEN=1\n",
            )),
            [
                pair("PRIVATE_KEY", None),
                pair("AFTER", Some("1")),
                pair("SINGLE", None),
            ]
        );
    }

    #[test]
    fn interpolation_stays_literal_and_never_reads_the_agent_environment() {
        // A variable that is certainly set in this test process.
        let name = "PATH";
        let path = std::env::var(name).expect("PATH is set for tests");
        let scan = parse(
            EnvExampleFile::Example,
            &format!("A=${{{name}}}\nB=\"${name}\"\nC=$(id)\nD=`whoami`\n"),
        );
        let values: Vec<_> = scan
            .example
            .entries
            .iter()
            .map(|entry| entry.example_value.clone())
            .collect();
        assert_eq!(
            values,
            [
                Some(format!("${{{name}}}")),
                Some(format!("${name}")),
                Some("$(id)".to_owned()),
                Some("whoami".to_owned()),
            ]
        );
        let json = serde_json::to_string(&scan.example).expect("json");
        assert!(!json.contains(&path));
    }

    #[test]
    fn invalid_keys_are_dropped_without_counting() {
        let long_key = format!("A{}", "B".repeat(MAX_KEY_CHARS));
        let max_key = "C".repeat(MAX_KEY_CHARS);
        assert_eq!(
            entries(&format!(
                "1ABC=x\nWITH-DASH=x\nWITH.DOT=x\nKÜNSTLER=x\n=novalue\n{long_key}=x\n{max_key}=ok\n_ok=1\n"
            )),
            [pair(&max_key, Some("ok")), pair("_ok", Some("1"))]
        );
    }

    #[test]
    fn more_than_the_entry_limit_is_truncated_in_file_order() {
        let contents: String = (0..=MAX_ENTRIES)
            .map(|index| format!("KEY_{index}=v\n"))
            .chain(std::iter::once("KEY_0=updated\n".to_owned()))
            .collect();
        let scan = parse(EnvExampleFile::Example, &contents);
        assert!(scan.example.truncated);
        assert_eq!(scan.example.entries.len(), MAX_ENTRIES);
        assert_eq!(
            scan.example.entries[0].example_value.as_deref(),
            Some("updated")
        );
        assert_eq!(
            scan.example.entries.last().map(|entry| entry.key.as_str()),
            Some("KEY_199")
        );

        let exact: String = (0..MAX_ENTRIES)
            .map(|index| format!("KEY_{index}=v\n"))
            .collect();
        assert!(!parse(EnvExampleFile::Example, &exact).example.truncated);
    }

    #[test]
    fn a_long_value_is_cut_on_a_character_boundary() {
        // `é` is two bytes, so a byte cut at 200 would land inside a character.
        let value = format!("a{}", "é".repeat(MAX_VALUE_CHARS));
        let parsed = entries(&format!("LONG={value}\n"));
        let [(_, Some(cut))] = &parsed[..] else {
            panic!("one entry with a value");
        };
        assert_eq!(cut.chars().count(), MAX_VALUE_CHARS);
        assert!(cut.starts_with("aé"));
    }

    #[test]
    fn values_that_look_like_credentials_are_null() {
        assert_eq!(
            entries(concat!(
                "DATABASE_URL=postgres://user:secret@db.example:5432/app\n",
                "REDIS_URL=redis://:secret@cache:6379\n",
                "ODD_URL=\"weird scheme://u:p@h/x y\"\n",
                "CERT=\"-----BEGIN CERTIFICATE-----\"\n",
                "NO_PASSWORD=postgres://user@db.example/app\n",
                "PLAIN=https://example.com/path?a=b\n",
            )),
            [
                pair("DATABASE_URL", None),
                pair("REDIS_URL", None),
                pair("ODD_URL", None),
                pair("CERT", None),
                pair("NO_PASSWORD", Some("postgres://user@db.example/app")),
                pair("PLAIN", Some("https://example.com/path?a=b")),
            ]
        );
    }

    #[test]
    fn the_engine_value_survives_a_hidden_credential() {
        let scan = parse(
            EnvExampleFile::Example,
            "DATABASE_URL=\"postgres://user:secret@db/app\"\nDB_CONNECTION=mysql\n",
        );
        assert_eq!(scan.example.entries[0].example_value, None);
        assert_eq!(
            scan.engine_value("DATABASE_URL"),
            Some("postgres://user:secret@db/app")
        );
        assert_eq!(scan.engine_value("DB_CONNECTION"), Some("mysql"));
        assert_eq!(scan.engine_value("DB_URL"), None);
    }

    #[test]
    fn crlf_and_a_byte_order_mark_are_handled() {
        assert_eq!(
            entries("\u{feff}A=1\r\nB=\"two\"\r\n"),
            [pair("A", Some("1")), pair("B", Some("two"))]
        );
    }

    async fn read_in(files: &[(&str, &[u8])]) -> Option<EnvExampleScan> {
        let temp = TempDir::new().expect("temp");
        for (name, contents) in files {
            std::fs::write(temp.path().join(name), contents).expect("file");
        }
        read(temp.path()).await
    }

    #[tokio::test]
    async fn dot_env_is_never_opened() {
        let scan = read_in(&[(".env", b"APP_KEY=base64:FAKE-SECRET-VALUE\n")]).await;
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn the_first_candidate_wins() {
        let scan = read_in(&[
            (".env", b"FROM_DOT_ENV=FAKE-SECRET\n"),
            (".env.dist", b"FROM_DIST=1\n"),
            (".env.sample", b"FROM_SAMPLE=1\n"),
        ])
        .await
        .expect("a candidate");
        assert_eq!(scan.example.file, EnvExampleFile::Sample);
        let json = serde_json::to_string(&scan.example).expect("json");
        assert!(json.contains("FROM_SAMPLE"));
        assert!(!json.contains("FAKE-SECRET") && !json.contains("FROM_DIST"));
    }

    #[tokio::test]
    async fn a_file_over_64_kib_is_not_read() {
        let mut contents = b"A=1\n".to_vec();
        contents.resize(65_537, b'#');
        assert_eq!(read_in(&[(".env.example", &contents)]).await, None);

        let mut exact = b"A=1\n".to_vec();
        exact.resize(65_536, b'#');
        assert!(read_in(&[(".env.example", &exact)]).await.is_some());
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_directory_is_null_and_blocks_later_candidates() {
        let outside = TempDir::new().expect("temp");
        std::fs::write(outside.path().join("secrets"), "TOKEN=FAKE-SECRET\n").expect("file");
        let temp = TempDir::new().expect("temp");
        std::os::unix::fs::symlink(
            outside.path().join("secrets"),
            temp.path().join(".env.example"),
        )
        .expect("symlink");
        std::fs::write(temp.path().join(".env.sample"), "LATER=1\n").expect("file");
        assert_eq!(read(temp.path()).await, None);
    }

    #[tokio::test]
    async fn a_symlink_inside_the_directory_and_a_directory_named_like_a_candidate() {
        let temp = TempDir::new().expect("temp");
        std::fs::write(temp.path().join("example.env"), "INSIDE=1\n").expect("file");
        std::os::unix::fs::symlink(
            temp.path().join("example.env"),
            temp.path().join(".env.example"),
        )
        .expect("symlink");
        let scan = read(temp.path()).await.expect("inside symlink is read");
        assert_eq!(scan.example.entries[0].key, "INSIDE");

        let directory = TempDir::new().expect("temp");
        std::fs::create_dir(directory.path().join(".env.example")).expect("dir");
        assert_eq!(read(directory.path()).await, None);
    }
}
