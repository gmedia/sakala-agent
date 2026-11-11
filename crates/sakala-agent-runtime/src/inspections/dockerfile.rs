//! Reads the TCP ports that the final stage of a Dockerfile declares with `EXPOSE`,
//! and which registry image that stage is built on.
//!
//! No maintained Rust crate parses Dockerfiles well enough for this. `dockerfile-parser`
//! is archived, and `dockerfile-parser-rs` rejects a whole file over one `HEALTHCHECK`,
//! `ONBUILD`, or lowercase instruction. Only `FROM`, `ARG`, `ENV`, and `EXPOSE` matter
//! here, so this reads just those, following the BuildKit rules for comments, line
//! continuations, heredocs, and variables, and skips whatever it cannot resolve. The
//! file is untrusted input: nothing here fails, and the output is only port numbers.

use std::collections::{BTreeSet, HashMap};

/// More entries than this cannot change what the API does with the list.
const MAX_PORTS: usize = 32;

/// What a Dockerfile says about the ports of the image it builds.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DockerfilePorts {
    /// TCP ports the final stage exposes, sorted and without duplicates.
    pub ports: Vec<u16>,
    /// The registry image under the final stage, following `FROM <stage>` back to
    /// the first stage built on something else, as written after variable
    /// substitution. Set only when no stage in that chain has an `EXPOSE`, because
    /// only then do the image's own ports decide. `None` for `scratch` and for a
    /// reference with a variable this reader cannot resolve.
    pub base_image: Option<String>,
}

/// Reads the final stage of `contents`.
pub fn read(contents: &str) -> DockerfilePorts {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let escape = escape_directive(contents);
    // A name missing from these maps has no value this reader can know.
    let mut global_args: HashMap<String, String> = HashMap::new();
    let mut stages: Vec<Stage> = Vec::new();

    for instruction in instructions(contents, escape) {
        match (instruction.keyword.as_str(), stages.last_mut()) {
            ("FROM", _) => {
                let stage =
                    Stage::from_instruction(&instruction.arguments, escape, &global_args, &stages);
                stages.push(stage);
            }
            ("ARG", None) => {
                for (name, default) in arguments(&instruction.arguments, escape) {
                    let value = default
                        .and_then(|raw| expand(&raw, escape, &|key| global_args.get(key).cloned()));
                    assign(&mut global_args, name, value);
                }
            }
            ("ARG", Some(stage)) => {
                stage.declare_args(&instruction.arguments, escape, &global_args);
            }
            ("ENV", Some(stage)) => stage.set_env(&instruction.arguments, escape),
            ("EXPOSE", Some(stage)) => stage.expose(&instruction.arguments, escape),
            _ => {}
        }
    }

    stages
        .pop()
        .map(|stage| DockerfilePorts {
            base_image: stage.base.filter(|_| !stage.declares_expose),
            ports: stage.ports.into_iter().take(MAX_PORTS).collect(),
        })
        .unwrap_or_default()
}

fn assign(variables: &mut HashMap<String, String>, name: String, value: Option<String>) {
    match value {
        Some(value) => variables.insert(name, value),
        None => variables.remove(&name),
    };
}

#[derive(Clone, Default)]
struct Stage {
    name: Option<String>,
    /// `ARG` and `ENV` values together: BuildKit lets the later of the two win.
    variables: HashMap<String, String>,
    ports: BTreeSet<u16>,
    /// Whether this stage, or one it is built on, has an `EXPOSE`, resolvable or not.
    declares_expose: bool,
    /// The registry image at the bottom of the chain, when it is known.
    base: Option<String>,
}

impl Stage {
    fn from_instruction(
        arguments: &str,
        escape: char,
        global_args: &HashMap<String, String>,
        earlier: &[Self],
    ) -> Self {
        let words = split_words(arguments, escape);
        let mut words = words.iter().filter(|word| !word.starts_with("--"));
        let raw = words.next();
        let expanded =
            raw.and_then(|raw| expand(raw, escape, &|key| global_args.get(key).cloned()));
        let image = raw.map(|raw| expanded.as_ref().unwrap_or(raw).to_ascii_lowercase());
        let name = match (words.next(), words.next()) {
            (Some(keyword), Some(name)) if keyword.eq_ignore_ascii_case("as") => {
                Some(name.to_ascii_lowercase())
            }
            _ => None,
        };

        // A stage built on an earlier one inherits its variables, exposed ports, and
        // base image. Anything else is an image from a registry, unless it is
        // `scratch` or still holds a variable nobody could resolve.
        let parent = image.and_then(|image| {
            earlier
                .iter()
                .rev()
                .find(|stage| stage.name.as_deref() == Some(image.as_str()))
        });
        match parent {
            Some(parent) => Self {
                name,
                ..parent.clone()
            },
            None => Self {
                name,
                base: expanded.filter(|image| {
                    !image.is_empty()
                        && !image.contains('$')
                        && !image.eq_ignore_ascii_case("scratch")
                }),
                ..Self::default()
            },
        }
    }

    fn lookup(&self, key: &str) -> Option<String> {
        self.variables.get(key).cloned()
    }

    /// `ARG NAME` takes the global default, if any; without one it leaves an
    /// earlier value alone.
    fn declare_args(
        &mut self,
        arguments: &str,
        escape: char,
        global_args: &HashMap<String, String>,
    ) {
        for (name, default) in self::arguments(arguments, escape) {
            match default {
                Some(raw) => {
                    let value = expand(&raw, escape, &|key| self.lookup(key));
                    assign(&mut self.variables, name, value);
                }
                None => {
                    if let Some(value) = global_args.get(&name) {
                        self.variables.insert(name, value.clone());
                    }
                }
            }
        }
    }

    fn set_env(&mut self, arguments: &str, escape: char) {
        let words = split_words(arguments, escape);
        let pairs: Vec<(String, Option<String>)> =
            if words.first().is_some_and(|word| word.contains('=')) {
                words
                    .iter()
                    .filter_map(|word| word.split_once('='))
                    .map(|(key, raw)| {
                        (
                            key.to_owned(),
                            expand(raw, escape, &|name| self.lookup(name)),
                        )
                    })
                    .collect()
            } else {
                // Legacy `ENV KEY value with spaces`.
                let trimmed = arguments.trim();
                let Some((key, raw)) = trimmed.split_once(char::is_whitespace) else {
                    return;
                };
                vec![(
                    key.to_owned(),
                    expand(raw.trim(), escape, &|name| self.lookup(name)),
                )]
            };
        // Every value on one line sees the variables from before that line.
        for (name, value) in pairs {
            assign(&mut self.variables, name, value);
        }
    }

    fn expose(&mut self, arguments: &str, escape: char) {
        self.declares_expose = true;
        for word in split_words(arguments, escape) {
            let Some(expanded) = expand(&word, escape, &|key| self.lookup(key)) else {
                continue;
            };
            for spec in expanded.split_whitespace() {
                for port in tcp_ports(spec) {
                    if self.ports.len() >= MAX_PORTS {
                        return;
                    }
                    self.ports.insert(port);
                }
            }
        }
    }
}

/// Ports of one `EXPOSE` item: `80`, `80/tcp`, or a range such as `8000-8010/tcp`.
/// UDP and SCTP ports are left out, because Sakala routes HTTP.
pub fn tcp_ports(spec: &str) -> Vec<u16> {
    let (ports, protocol) = spec.split_once('/').unwrap_or((spec, "tcp"));
    if !protocol.eq_ignore_ascii_case("tcp") {
        return Vec::new();
    }
    let parse = |value: &str| value.parse::<u16>().ok().filter(|port| *port > 0);
    match ports.split_once('-') {
        Some((start, end)) => match (parse(start), parse(end)) {
            (Some(start), Some(end)) if start <= end => (start..=end).take(MAX_PORTS).collect(),
            _ => Vec::new(),
        },
        None => parse(ports).into_iter().collect(),
    }
}

/// `ARG NAME`, `ARG NAME=default`, possibly several on one line.
fn arguments(arguments: &str, escape: char) -> Vec<(String, Option<String>)> {
    split_words(arguments, escape)
        .into_iter()
        .map(|word| match word.split_once('=') {
            Some((name, default)) => (name.to_owned(), Some(default.to_owned())),
            None => (word, None),
        })
        .collect()
}

struct Instruction {
    keyword: String,
    arguments: String,
}

/// `# escape=` must come before anything else, among the other parser directives.
fn escape_directive(contents: &str) -> char {
    for line in contents.lines() {
        let Some(directive) = line.trim().strip_prefix('#') else {
            break;
        };
        let Some((key, value)) = directive.split_once('=') else {
            break;
        };
        if key.trim().eq_ignore_ascii_case("escape") {
            return if value.trim() == "`" { '`' } else { '\\' };
        }
    }
    '\\'
}

/// Joins continued lines, drops comments, and skips heredoc bodies.
fn instructions(contents: &str, escape: char) -> Vec<Instruction> {
    let mut lines = contents.lines();
    let mut instructions = Vec::new();

    while let Some(first) = lines.next() {
        let first = first.trim_start();
        if first.is_empty() || first.starts_with('#') {
            continue;
        }
        let mut logical = String::new();
        let mut line = first;
        loop {
            match strip_continuation(line, escape) {
                Some(continued) => logical.push_str(continued),
                None => {
                    logical.push_str(line);
                    break;
                }
            }
            // Comment and empty lines inside a continuation are skipped.
            let next = lines.by_ref().find(|next| {
                let trimmed = next.trim_start();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            });
            match next {
                Some(next) => line = next,
                None => break,
            }
        }

        let (keyword, arguments) = logical
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((logical.trim(), ""));
        let keyword = keyword.to_ascii_uppercase();
        if matches!(keyword.as_str(), "RUN" | "COPY" | "ADD") {
            for (delimiter, strip_tabs) in heredoc_delimiters(arguments) {
                for body in lines.by_ref() {
                    let body = if strip_tabs {
                        body.trim_start_matches('\t')
                    } else {
                        body
                    };
                    if body == delimiter {
                        break;
                    }
                }
            }
        }
        instructions.push(Instruction {
            keyword,
            arguments: arguments.to_owned(),
        });
    }
    instructions
}

fn strip_continuation(line: &str, escape: char) -> Option<&str> {
    line.trim_end_matches([' ', '\t']).strip_suffix(escape)
}

/// `<<EOF`, `<<-EOF`, `<<"EOF"`, `3<<'EOF'`: the delimiters, in order, and whether
/// leading tabs are stripped from the body.
fn heredoc_delimiters(arguments: &str) -> Vec<(String, bool)> {
    arguments
        .split_whitespace()
        .filter_map(|word| {
            let marker = word.trim_start_matches(|c: char| c.is_ascii_digit());
            let rest = marker.strip_prefix("<<")?;
            let (rest, strip_tabs) = match rest.strip_prefix('-') {
                Some(rest) => (rest, true),
                None => (rest, false),
            };
            let delimiter = ['"', '\'']
                .iter()
                .find_map(|quote| rest.strip_prefix(*quote)?.strip_suffix(*quote))
                .unwrap_or(rest);
            let valid = delimiter
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && delimiter
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');
            valid.then(|| (delimiter.to_owned(), strip_tabs))
        })
        .collect()
}

/// Splits on whitespace outside quotes, keeping quotes and escapes for [`expand`].
fn split_words(arguments: &str, escape: char) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = arguments.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some(open) => {
                word.push(c);
                if c == escape && open == '"' {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                } else if c == open {
                    quote = None;
                }
            }
            None if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            None => {
                word.push(c);
                if c == escape {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                } else if c == '"' || c == '\'' {
                    quote = Some(c);
                }
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Removes quotes and escapes and substitutes `$NAME`, `${NAME}`, `${NAME:-word}`,
/// and `${NAME:+word}`. `None` when a variable cannot be resolved or the syntax is
/// one this reader does not follow.
fn expand(word: &str, escape: char, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut output = String::new();
    let mut chars = word.chars().peekable();
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                for literal in chars.by_ref() {
                    if literal == '\'' {
                        break;
                    }
                    output.push(literal);
                }
            }
            '"' => in_double = !in_double,
            c if c == escape => {
                if let Some(next) = chars.next() {
                    output.push(next);
                }
            }
            '$' => match chars.peek() {
                Some('{') => {
                    chars.next();
                    let mut body = String::new();
                    let mut depth = 1;
                    for inner in chars.by_ref() {
                        match inner {
                            '{' => depth += 1,
                            '}' => depth -= 1,
                            _ => {}
                        }
                        if depth == 0 {
                            break;
                        }
                        body.push(inner);
                    }
                    if depth != 0 {
                        return None;
                    }
                    output.push_str(&braced(&body, escape, lookup)?);
                }
                Some(next) if next.is_ascii_alphabetic() || *next == '_' => {
                    let mut name = String::new();
                    while let Some(next) = chars.peek() {
                        if !(next.is_ascii_alphanumeric() || *next == '_') {
                            break;
                        }
                        name.push(*next);
                        chars.next();
                    }
                    output.push_str(&lookup(&name)?);
                }
                _ => output.push('$'),
            },
            c => output.push(c),
        }
    }
    Some(output)
}

fn braced(body: &str, escape: char, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let name_end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(body.len());
    let (name, modifier) = body.split_at(name_end);
    if name.is_empty() {
        return None;
    }
    let value = lookup(name).filter(|value| !value.is_empty());
    if modifier.is_empty() {
        return lookup(name);
    }
    if let Some(word) = modifier.strip_prefix(":-") {
        return match value {
            Some(value) => Some(value),
            None => expand(word, escape, lookup),
        };
    }
    if let Some(word) = modifier.strip_prefix(":+") {
        return match value {
            Some(_) => expand(word, escape, lookup),
            None => Some(String::new()),
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::read;

    fn ports(dockerfile: &str) -> Vec<u16> {
        read(dockerfile).ports
    }

    fn base(dockerfile: &str) -> Option<String> {
        read(dockerfile).base_image
    }

    #[test]
    fn a_single_expose_is_read() {
        assert_eq!(
            ports("FROM nginx:alpine\nCOPY . /usr/share/nginx/html\nEXPOSE 80\n"),
            [80]
        );
    }

    #[test]
    fn the_tcp_suffix_is_accepted_and_udp_is_left_out() {
        assert_eq!(ports("FROM scratch\nEXPOSE 80/tcp\n"), [80]);
        assert_eq!(ports("FROM scratch\nEXPOSE 80/TCP 53/udp 9/sctp\n"), [80]);
        assert!(ports("FROM scratch\nEXPOSE 53/udp\n").is_empty());
    }

    #[test]
    fn several_ports_are_sorted_and_deduplicated() {
        assert_eq!(
            ports("FROM scratch\nEXPOSE 443 80\nEXPOSE 80/tcp\n"),
            [80, 443]
        );
        assert_eq!(
            ports("FROM scratch\nEXPOSE 8000-8002\n"),
            [8000, 8001, 8002]
        );
    }

    #[test]
    fn a_large_range_is_bounded() {
        assert_eq!(ports("FROM scratch\nEXPOSE 1-65535\n").len(), 32);
    }

    #[test]
    fn only_the_final_stage_counts() {
        let dockerfile = "\
FROM node:22 AS build
EXPOSE 3000
RUN npm run build

FROM nginx:alpine
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 80
";
        assert_eq!(ports(dockerfile), [80]);
        assert!(ports("FROM node:22 AS build\nEXPOSE 3000\nFROM nginx:alpine\n").is_empty());
    }

    #[test]
    fn a_stage_built_on_an_earlier_stage_inherits_its_ports_and_environment() {
        let dockerfile = "\
FROM node:22 AS Base
ENV PORT=8080
EXPOSE 9229
FROM base
EXPOSE $PORT
";
        assert_eq!(ports(dockerfile), [8080, 9229]);
    }

    #[test]
    fn arg_and_env_values_are_substituted() {
        assert_eq!(ports("FROM scratch\nARG PORT=8080\nEXPOSE $PORT\n"), [8080]);
        assert_eq!(
            ports("FROM scratch\nENV PORT=5000\nEXPOSE ${PORT}/tcp\n"),
            [5000]
        );
        assert_eq!(
            ports("FROM scratch\nENV PORT 5000\nEXPOSE ${PORT}\n"),
            [5000]
        );
        assert_eq!(ports("FROM scratch\nEXPOSE ${PORT:-8000}\n"), [8000]);
        assert_eq!(
            ports("FROM scratch\nENV A=1 PORT=\"7000\"\nEXPOSE $PORT\n"),
            [7000]
        );
        assert_eq!(
            ports("FROM scratch\nENV HTTP=80\nENV PORTS=\"$HTTP 443\"\nEXPOSE $PORTS\n"),
            [80, 443]
        );
        // Values on one line see the variables from before that line.
        assert_eq!(
            ports("FROM scratch\nENV P=1\nENV P=80 Q=$P\nEXPOSE $Q\n"),
            [1]
        );
    }

    #[test]
    fn the_later_of_arg_and_env_wins_as_in_buildkit() {
        assert_eq!(
            ports("FROM scratch\nENV PORT=80\nARG PORT=3000\nEXPOSE $PORT\n"),
            [3000]
        );
        assert_eq!(
            ports("FROM scratch\nARG PORT=3000\nENV PORT=80\nEXPOSE $PORT\n"),
            [80]
        );
        assert_eq!(
            ports("FROM scratch\nENV PORT=80\nARG PORT\nEXPOSE $PORT\n"),
            [80]
        );
        assert_eq!(
            ports("ARG PORT=9000\nFROM scratch\nENV PORT=80\nARG PORT\nEXPOSE $PORT\n"),
            [9000]
        );
        assert_eq!(
            ports("FROM scratch AS a\nARG P=1\nENV E=2\nFROM a\nEXPOSE $P $E\n"),
            [1, 2]
        );
    }

    #[test]
    fn a_global_arg_needs_to_be_declared_again_in_the_stage() {
        assert!(ports("ARG PORT=8080\nFROM scratch\nEXPOSE $PORT\n").is_empty());
        assert_eq!(
            ports("ARG PORT=8080\nFROM scratch\nARG PORT\nEXPOSE $PORT\n"),
            [8080]
        );
        assert_eq!(
            ports("ARG BASE=web\nFROM scratch AS web\nEXPOSE 81\nFROM ${BASE}\n"),
            [81]
        );
    }

    #[test]
    fn an_unresolvable_value_is_ignored() {
        assert!(ports("FROM node:22\nEXPOSE $PORT\n").is_empty());
        assert!(ports("FROM node:22\nARG PORT\nEXPOSE $PORT\n").is_empty());
        assert_eq!(ports("FROM node:22\nEXPOSE $PORT 80\n"), [80]);
        assert!(ports("FROM scratch\nARG P=80\nEXPOSE ${P%0}\n").is_empty());
        assert!(ports("FROM scratch\nEXPOSE http\n").is_empty());
        assert!(ports("FROM scratch\nEXPOSE 0 70000\n").is_empty());
    }

    #[test]
    fn single_quotes_and_escapes_are_literal() {
        assert!(ports("FROM scratch\nENV P=80\nEXPOSE '$P'\n").is_empty());
        assert!(ports("FROM scratch\nENV P=80\nEXPOSE \\$P\n").is_empty());
    }

    #[test]
    fn comments_continuations_and_case_follow_buildkit() {
        let dockerfile = "\
# EXPOSE 1
from scratch
  # EXPOSE 2
expose 80 \\
  # a comment inside the continuation
  443\\

/tcp
";
        assert_eq!(ports(dockerfile), [80, 443]);
    }

    #[test]
    fn the_escape_directive_changes_the_continuation_character() {
        assert_eq!(
            ports("# escape=`\nFROM scratch\nEXPOSE 80 `\n 8080\n"),
            [80, 8080]
        );
    }

    #[test]
    fn heredoc_bodies_are_not_instructions() {
        let dockerfile = "\
FROM scratch
RUN <<EOF
EXPOSE 1
EOF
COPY <<-\"CONF\" /etc/app.conf
\tEXPOSE 2
\tCONF
EXPOSE 80
";
        assert_eq!(ports(dockerfile), [80]);
    }

    #[test]
    fn instructions_this_reader_does_not_follow_change_nothing() {
        let dockerfile = "\
FROM scratch
HEALTHCHECK CMD wget -q -O- http://localhost/ || exit 1
ONBUILD EXPOSE 9000
MAINTAINER someone
EXPOSE 80
";
        assert_eq!(ports(dockerfile), [80]);
    }

    #[test]
    fn crlf_and_a_byte_order_mark_are_tolerated() {
        assert_eq!(ports("\u{feff}FROM scratch\r\nEXPOSE 80\r\n"), [80]);
    }

    #[test]
    fn nothing_without_a_stage() {
        assert!(ports("").is_empty());
        assert!(ports("EXPOSE 80\n").is_empty());
        assert!(ports("ARG X=1\n").is_empty());
    }

    #[test]
    fn the_base_image_is_reported_when_the_final_stage_exposes_nothing() {
        assert_eq!(
            base("FROM nginx:alpine\nCOPY . /usr/share/nginx/html\n").as_deref(),
            Some("nginx:alpine")
        );
        let dockerfile = "\
FROM node:22 AS build
EXPOSE 3000
RUN npm run build
FROM --platform=linux/amd64 httpd:2.4-alpine
COPY --from=build /app/dist /usr/local/apache2/htdocs
";
        assert_eq!(base(dockerfile).as_deref(), Some("httpd:2.4-alpine"));
        assert!(ports(dockerfile).is_empty());
    }

    #[test]
    fn the_base_image_keeps_the_case_of_its_tag() {
        assert_eq!(
            base("FROM example/app:Release-1\n").as_deref(),
            Some("example/app:Release-1")
        );
    }

    #[test]
    fn a_chain_of_stages_leads_back_to_the_registry_image() {
        let dockerfile = "\
FROM nginx:1.27 AS Base
RUN echo base
FROM base AS assets
COPY . /usr/share/nginx/html
FROM ASSETS
";
        assert_eq!(base(dockerfile).as_deref(), Some("nginx:1.27"));
        // A name that only a later stage takes is an image, as in BuildKit.
        assert_eq!(
            base("FROM web\nFROM nginx AS web\nFROM node:22 AS other\nFROM web\n").as_deref(),
            Some("nginx")
        );
    }

    #[test]
    fn an_expose_anywhere_in_the_chain_means_the_dockerfile_decides() {
        assert_eq!(base("FROM nginx:alpine\nEXPOSE 8080\n"), None);
        assert_eq!(
            base("FROM nginx:alpine AS web\nEXPOSE 8080\nFROM web\n"),
            None
        );
        // Even one this reader cannot resolve, or one that is not TCP.
        assert_eq!(base("FROM nginx:alpine\nEXPOSE $PORT\n"), None);
        assert_eq!(base("FROM nginx:alpine\nEXPOSE 53/udp\n"), None);
        // An EXPOSE in an unrelated stage does not count.
        assert_eq!(
            base("FROM node:22 AS build\nEXPOSE 3000\nFROM nginx\n").as_deref(),
            Some("nginx")
        );
    }

    #[test]
    fn scratch_and_unresolved_references_have_no_base_image() {
        assert_eq!(base("FROM scratch\n"), None);
        assert_eq!(base("FROM SCRATCH AS out\nFROM out\n"), None);
        assert_eq!(base("FROM nginx:${VERSION}\n"), None);
        assert_eq!(base("ARG VERSION\nFROM nginx:${VERSION}\n"), None);
        assert_eq!(base("FROM $BASE_IMAGE\n"), None);
        assert_eq!(base("FROM nginx:$1\n"), None);
        assert_eq!(base("FROM\n"), None);
        assert_eq!(base(""), None);
    }

    #[test]
    fn a_global_arg_with_a_default_resolves_the_base_image() {
        assert_eq!(
            base("ARG NGINX=1.27-alpine\nFROM nginx:${NGINX}\n").as_deref(),
            Some("nginx:1.27-alpine")
        );
        assert_eq!(
            base("FROM ${IMAGE:-httpd:2.4}\n").as_deref(),
            Some("httpd:2.4")
        );
    }
}
