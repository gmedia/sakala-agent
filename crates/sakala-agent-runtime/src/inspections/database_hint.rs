//! Looks for evidence that a project uses a database, which engine, and which
//! migration command to suggest as its release command.
//!
//! Every file here is repository content read on the host with the bounded rules
//! of [`repository_file`]: inside the inspected directory only, regular files, at
//! most 1 MiB each, directory listings capped. Nothing is executed, unreadable files
//! are skipped silently, and no content is logged. The checks are line scans and
//! JSON reads of well-known files; a project they miss only loses a suggestion.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use sakala_agent_protocol::{
    DatabaseEngine, DatabaseHint, DatabaseSignal, DatabaseSignalKind, SuggestedReleaseCommand,
};
use serde_json::Value;
use tokio::fs;

use super::env_example::EnvExampleScan;
use crate::repository_file::{self, EntryKind, MAX_MANIFEST_BYTES};

const MAX_SIGNALS: usize = 16;
/// Entries looked at in one directory listing.
const MAX_DIRECTORY_ENTRIES: usize = 200;
/// Django settings files read at most, across every candidate directory.
const MAX_DJANGO_SETTINGS_FILES: usize = 16;
/// A signal name taken from a file (Prisma provider, Rails adapter) longer than
/// this, or with other characters, is reported as `null`.
const MAX_SIGNAL_NAME_CHARS: usize = 64;

/// Keys in the env example that point at a database.
const DATABASE_ENV_KEYS: [&str; 9] = [
    "DATABASE_URL",
    "DB_URL",
    "DB_CONNECTION",
    "DB_HOST",
    "POSTGRES_URL",
    "POSTGRES_HOST",
    "MYSQL_URL",
    "MYSQL_HOST",
    "PGHOST",
];
const DB_CONNECTION_KEY: &str = "DB_CONNECTION";
/// Connection URLs whose scheme names the engine, in priority order.
const DATABASE_URL_KEYS: [&str; 2] = ["DATABASE_URL", "DB_URL"];

const PACKAGE_JSON: &str = "package.json";
const PACKAGE_JSON_SECTIONS: [&str; 2] = ["dependencies", "devDependencies"];
const NODE_DATABASE_PACKAGES: [&str; 13] = [
    "pg",
    "postgres",
    "@prisma/client",
    "prisma",
    "drizzle-orm",
    "sequelize",
    "typeorm",
    "knex",
    "mysql",
    "mysql2",
    "better-sqlite3",
    "sqlite3",
    "@neondatabase/serverless",
];
const PRISMA_PACKAGES: [&str; 2] = ["prisma", "@prisma/client"];

const COMPOSER_JSON: &str = "composer.json";
const COMPOSER_REQUIRE: &str = "require";
const COMPOSER_SECTIONS: [&str; 2] = [COMPOSER_REQUIRE, "require-dev"];
const COMPOSER_DATABASE_PACKAGES: [&str; 2] = ["doctrine/orm", "doctrine/dbal"];
const LARAVEL_PACKAGE: &str = "laravel/framework";
const LARAVEL_MIGRATIONS: &str = "database/migrations";
const PHP_EXTENSION: &str = ".php";
const ARTISAN: &str = "artisan";

const PYTHON_MANIFESTS: [&str; 2] = ["requirements.txt", "pyproject.toml"];
const PYPROJECT_TOML: &str = "pyproject.toml";
/// Normalized names (lowercase, `-` for `_` and `.`).
const PYTHON_DATABASE_PACKAGES: [&str; 8] = [
    "psycopg",
    "psycopg2",
    "psycopg2-binary",
    "asyncpg",
    "dj-database-url",
    "sqlalchemy",
    "mysqlclient",
    "pymysql",
];
const DJANGO_PACKAGE: &str = "django";
const MANAGE_PY: &str = "manage.py";
const DJANGO_SETTINGS_FILE: &str = "settings.py";
const DJANGO_SETTINGS_PACKAGE: &str = "settings";
const PYTHON_EXTENSION: &str = ".py";
/// Settings markers; the first one present names the signal.
const DJANGO_MARKERS: [&str; 2] = ["dj_database_url", "DATABASES"];
const DJANGO_BACKEND_PREFIX: &str = "django.db.backends.";

const GEMFILE: &str = "Gemfile";
const GEM_KEYWORD: &str = "gem";
const RUBY_DATABASE_GEMS: [&str; 4] = ["pg", "mysql2", "sqlite3", "trilogy"];
const RAILS_DATABASE_CONFIG: &str = "config/database.yml";
const RAILS_ADAPTER_KEY: &str = "adapter:";
const RAILS_MIGRATIONS: &str = "db/migrate";
const BIN_RAILS: &str = "bin/rails";
/// ERB in `database.yml`, whose value is only known at runtime.
const ERB_OPEN: &str = "<%";

/// The schema locations Prisma looks at by default, in its order.
const PRISMA_SCHEMAS: [&str; 2] = ["prisma/schema.prisma", "schema.prisma"];
const PRISMA_MIGRATIONS: &str = "prisma/migrations";
const PRISMA_DATASOURCE: &str = "datasource";
const PRISMA_PROVIDER: &str = "provider";
const PRISMA_COMMENT: &str = "//";

pub(super) async fn detect(context: &Path, env: Option<&EnvExampleScan>) -> DatabaseHint {
    let Ok(root) = fs::canonicalize(context).await else {
        return DatabaseHint::none();
    };
    let files = ProjectFiles { root: &root };

    let env_signals = env.map(env_signals).unwrap_or_default();
    let composer = files.read_json(COMPOSER_JSON).await;
    let laravel_migrations = files.laravel_migrations(composer.as_ref()).await;
    let prisma = files.prisma().await;
    let django = files.django().await;
    let rails = files.rails().await;
    let package_json = files.read_json(PACKAGE_JSON).await;
    let python = files.python_packages().await;
    let mut packages = Vec::new();
    packages.extend(json_packages(
        package_json.as_ref(),
        PACKAGE_JSON,
        &PACKAGE_JSON_SECTIONS,
        &NODE_DATABASE_PACKAGES,
    ));
    packages.extend(json_packages(
        composer.as_ref(),
        COMPOSER_JSON,
        &COMPOSER_SECTIONS,
        &COMPOSER_DATABASE_PACKAGES,
    ));
    for (file, names) in &python.per_file {
        packages.extend(
            PYTHON_DATABASE_PACKAGES
                .iter()
                .filter(|package| names.contains(**package))
                .map(|package| package_signal(file, package)),
        );
    }
    packages.extend(files.ruby_gems().await);

    let engine = env
        .and_then(connection_engine)
        .or_else(|| env.and_then(url_engine))
        .or_else(|| prisma.as_ref().and_then(|found| found.engine))
        .or_else(|| rails.as_ref().and_then(|found| found.adapter_engine()))
        .or(django.as_ref().and_then(|found| found.engine))
        .or_else(|| {
            packages
                .iter()
                .find_map(|signal| signal.name.as_deref().and_then(package_engine))
        })
        .unwrap_or(DatabaseEngine::Unknown);

    let suggested_release_command = if laravel_migrations.is_some() && files.is_file(ARTISAN).await
    {
        Some(SuggestedReleaseCommand::LaravelMigrate)
    } else if python.has(DJANGO_PACKAGE) && files.is_file(MANAGE_PY).await {
        Some(SuggestedReleaseCommand::DjangoMigrate)
    } else if prisma.is_some()
        && has_any_package(package_json.as_ref(), &PRISMA_PACKAGES)
        && files.is_directory(PRISMA_MIGRATIONS).await
    {
        Some(SuggestedReleaseCommand::PrismaMigrateDeploy)
    } else if rails.is_some()
        && files.is_file(BIN_RAILS).await
        && files.is_directory(RAILS_MIGRATIONS).await
    {
        Some(SuggestedReleaseCommand::RailsMigrate)
    } else {
        None
    };

    // Framework evidence comes before package names, so the cap drops the latter.
    let mut signals = Vec::new();
    let mut seen = HashSet::new();
    let candidates = env_signals
        .into_iter()
        .chain(laravel_migrations)
        .chain(prisma.map(|found| found.signal))
        .chain(django.map(|found| found.signal))
        .chain(rails.map(|found| found.signal))
        .chain(packages);
    for signal in candidates {
        if signals.len() == MAX_SIGNALS {
            break;
        }
        if seen.insert(signal.clone()) {
            signals.push(signal);
        }
    }

    DatabaseHint {
        likely: !signals.is_empty(),
        engine,
        signals,
        suggested_release_command,
    }
}

/// Reads below the canonical inspected directory, never outside it.
struct ProjectFiles<'a> {
    root: &'a Path,
}

impl ProjectFiles<'_> {
    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    async fn read_text(&self, relative: &str) -> Option<String> {
        repository_file::read_text(self.root, &self.path(relative), MAX_MANIFEST_BYTES).await
    }

    async fn read_json(&self, relative: &str) -> Option<Value> {
        serde_json::from_str(&self.read_text(relative).await?).ok()
    }

    async fn is_file(&self, relative: &str) -> bool {
        repository_file::is_file_inside(self.root, &self.path(relative)).await
    }

    async fn is_directory(&self, relative: &str) -> bool {
        repository_file::is_directory_inside(self.root, &self.path(relative)).await
    }

    async fn list(&self, relative: &str) -> Vec<repository_file::DirectoryEntry> {
        repository_file::list_directory(self.root, &self.path(relative), MAX_DIRECTORY_ENTRIES)
            .await
    }

    /// `composer.json` requires Laravel and `database/migrations` holds a `*.php`.
    async fn laravel_migrations(&self, composer: Option<&Value>) -> Option<DatabaseSignal> {
        let requires_laravel = composer
            .and_then(|composer| composer.get(COMPOSER_REQUIRE))
            .and_then(Value::as_object)
            .is_some_and(|require| require.contains_key(LARAVEL_PACKAGE));
        if !requires_laravel {
            return None;
        }
        self.list(LARAVEL_MIGRATIONS)
            .await
            .iter()
            .any(|entry| entry.kind == EntryKind::File && entry.name.ends_with(PHP_EXTENSION))
            .then(|| DatabaseSignal {
                kind: DatabaseSignalKind::LaravelMigrations,
                file: LARAVEL_MIGRATIONS.to_owned(),
                name: None,
            })
    }

    async fn prisma(&self) -> Option<PrismaSchema> {
        for schema in PRISMA_SCHEMAS {
            let Some(contents) = self.read_text(schema).await else {
                continue;
            };
            let provider = prisma_provider(&contents)?;
            return Some(PrismaSchema {
                engine: prisma_engine(provider),
                signal: DatabaseSignal {
                    kind: DatabaseSignalKind::PrismaSchema,
                    file: schema.to_owned(),
                    name: signal_name(provider),
                },
            });
        }
        None
    }

    /// A settings module one level below `manage.py`: `<package>/settings.py` or
    /// `<package>/settings/*.py`, the first that mentions a database.
    async fn django(&self) -> Option<DjangoSettings> {
        if !self.is_file(MANAGE_PY).await {
            return None;
        }
        let mut read = 0;
        for package in self.list("").await {
            if package.kind != EntryKind::Directory || package.name.starts_with('.') {
                continue;
            }
            let mut candidates = vec![format!("{}/{DJANGO_SETTINGS_FILE}", package.name)];
            let settings_package = format!("{}/{DJANGO_SETTINGS_PACKAGE}", package.name);
            candidates.extend(
                self.list(&settings_package)
                    .await
                    .into_iter()
                    .filter(|entry| {
                        entry.kind == EntryKind::File && entry.name.ends_with(PYTHON_EXTENSION)
                    })
                    .map(|entry| format!("{settings_package}/{}", entry.name)),
            );
            for candidate in candidates {
                if read == MAX_DJANGO_SETTINGS_FILES {
                    return None;
                }
                if !self.is_file(&candidate).await {
                    continue;
                }
                read += 1;
                let Some(contents) = self.read_text(&candidate).await else {
                    continue;
                };
                if let Some(marker) = DJANGO_MARKERS
                    .iter()
                    .find(|marker| contents.contains(**marker))
                {
                    return Some(DjangoSettings {
                        engine: django_engine(&contents),
                        signal: DatabaseSignal {
                            kind: DatabaseSignalKind::DjangoSettings,
                            file: candidate,
                            name: Some((*marker).to_owned()),
                        },
                    });
                }
            }
        }
        None
    }

    async fn rails(&self) -> Option<RailsConfig> {
        if !self.is_file(RAILS_DATABASE_CONFIG).await {
            return None;
        }
        let adapter = self
            .read_text(RAILS_DATABASE_CONFIG)
            .await
            .and_then(|contents| rails_adapter(&contents));
        Some(RailsConfig {
            signal: DatabaseSignal {
                kind: DatabaseSignalKind::RailsDatabaseConfig,
                file: RAILS_DATABASE_CONFIG.to_owned(),
                name: adapter,
            },
        })
    }

    async fn python_packages(&self) -> PythonPackages {
        let mut per_file = Vec::new();
        for manifest in PYTHON_MANIFESTS {
            if let Some(contents) = self.read_text(manifest).await {
                per_file.push((
                    manifest,
                    python_names(&contents, manifest == PYPROJECT_TOML),
                ));
            }
        }
        PythonPackages { per_file }
    }

    async fn ruby_gems(&self) -> Vec<DatabaseSignal> {
        let Some(contents) = self.read_text(GEMFILE).await else {
            return Vec::new();
        };
        let gems: HashSet<&str> = contents.lines().filter_map(gem_name).collect();
        RUBY_DATABASE_GEMS
            .iter()
            .filter(|gem| gems.contains(**gem))
            .map(|gem| package_signal(GEMFILE, gem))
            .collect()
    }
}

struct PrismaSchema {
    engine: Option<DatabaseEngine>,
    signal: DatabaseSignal,
}

struct DjangoSettings {
    engine: Option<DatabaseEngine>,
    signal: DatabaseSignal,
}

struct RailsConfig {
    signal: DatabaseSignal,
}

impl RailsConfig {
    fn adapter_engine(&self) -> Option<DatabaseEngine> {
        match self.signal.name.as_deref()? {
            "postgresql" => Some(DatabaseEngine::Postgresql),
            "mysql2" | "trilogy" => Some(DatabaseEngine::Mysql),
            "sqlite3" => Some(DatabaseEngine::Sqlite),
            _ => None,
        }
    }
}

struct PythonPackages {
    per_file: Vec<(&'static str, HashSet<String>)>,
}

impl PythonPackages {
    fn has(&self, package: &str) -> bool {
        self.per_file
            .iter()
            .any(|(_, names)| names.contains(package))
    }
}

fn env_signals(env: &EnvExampleScan) -> Vec<DatabaseSignal> {
    env.example
        .entries
        .iter()
        .filter(|entry| DATABASE_ENV_KEYS.contains(&entry.key.as_str()))
        .map(|entry| DatabaseSignal {
            kind: DatabaseSignalKind::EnvVariable,
            file: env.example.file.as_str().to_owned(),
            name: Some(entry.key.clone()),
        })
        .collect()
}

fn connection_engine(env: &EnvExampleScan) -> Option<DatabaseEngine> {
    match env
        .engine_value(DB_CONNECTION_KEY)?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "pgsql" | "postgres" | "postgresql" => Some(DatabaseEngine::Postgresql),
        "mysql" | "mariadb" => Some(DatabaseEngine::Mysql),
        "sqlite" => Some(DatabaseEngine::Sqlite),
        _ => None,
    }
}

/// The scheme of the first connection URL that names an engine. A driver suffix
/// such as SQLAlchemy's `postgresql+psycopg` names the same engine.
fn url_engine(env: &EnvExampleScan) -> Option<DatabaseEngine> {
    DATABASE_URL_KEYS.iter().find_map(|key| {
        let (scheme, _) = env.engine_value(key)?.trim().split_once(':')?;
        let scheme = scheme.split('+').next()?.to_ascii_lowercase();
        match scheme.as_str() {
            "postgres" | "postgresql" => Some(DatabaseEngine::Postgresql),
            "mysql" | "mariadb" => Some(DatabaseEngine::Mysql),
            "sqlite" | "file" => Some(DatabaseEngine::Sqlite),
            _ => None,
        }
    })
}

/// `provider = "..."` inside the first `datasource` block.
fn prisma_provider(contents: &str) -> Option<&str> {
    let mut in_datasource = false;
    for line in contents.lines() {
        let line = line
            .split_once(PRISMA_COMMENT)
            .map_or(line, |(code, _)| code)
            .trim();
        if !in_datasource {
            in_datasource = line
                .strip_prefix(PRISMA_DATASOURCE)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace) && rest.ends_with('{'));
            continue;
        }
        if line.starts_with('}') {
            return None;
        }
        let Some(value) = line
            .strip_prefix(PRISMA_PROVIDER)
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        else {
            continue;
        };
        let value = value.trim().strip_prefix('"')?;
        return value.split_once('"').map(|(provider, _)| provider);
    }
    None
}

fn prisma_engine(provider: &str) -> Option<DatabaseEngine> {
    match provider {
        "postgresql" | "postgres" => Some(DatabaseEngine::Postgresql),
        "mysql" => Some(DatabaseEngine::Mysql),
        "sqlite" => Some(DatabaseEngine::Sqlite),
        _ => None,
    }
}

/// The engine of the first `django.db.backends.<name>` in a settings file.
fn django_engine(contents: &str) -> Option<DatabaseEngine> {
    let (_, rest) = contents.split_once(DJANGO_BACKEND_PREFIX)?;
    let backend: String = rest
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    if backend.starts_with("postgresql") {
        Some(DatabaseEngine::Postgresql)
    } else if backend == "mysql" {
        Some(DatabaseEngine::Mysql)
    } else if backend == "sqlite3" {
        Some(DatabaseEngine::Sqlite)
    } else {
        None
    }
}

/// The first `adapter:` value in `database.yml`. ERB has no value before runtime.
fn rails_adapter(contents: &str) -> Option<String> {
    let line = contents
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(RAILS_ADAPTER_KEY))?;
    let value = line[RAILS_ADAPTER_KEY.len()..].trim();
    if value.contains(ERB_OPEN) {
        return None;
    }
    let value = value
        .split_once(" #")
        .map_or(value, |(value, _)| value)
        .trim()
        .trim_matches(['"', '\'']);
    signal_name(value)
}

/// Names of packages in `requirements.txt`, or in `pyproject.toml` where a line may
/// also hold an inline array of requirement strings. Names are normalized.
fn python_names(contents: &str, pyproject: bool) -> HashSet<String> {
    let mut names = HashSet::new();
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', '-', '[']) {
            continue;
        }
        names.extend(requirement_name(line.trim_start_matches(['"', '\''])));
        if pyproject {
            names.extend(
                line.split(['"', '\''])
                    .skip(1)
                    .step_by(2)
                    .filter_map(requirement_name),
            );
        }
    }
    names
}

/// The distribution name before any extra, version, or marker, normalized like
/// PEP 503 (lowercase, `-` for `_` and `.`).
fn requirement_name(text: &str) -> Option<String> {
    let name: String = text
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .map(|character| match character {
            '_' | '.' => '-',
            other => other.to_ascii_lowercase(),
        })
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The gem in a `gem 'name'` or `gem("name")` line.
fn gem_name(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix(GEM_KEYWORD)?;
    if !rest.starts_with(|character: char| character.is_whitespace() || character == '(') {
        return None;
    }
    let rest =
        rest.trim_start_matches(|character: char| character.is_whitespace() || character == '(');
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    rest[1..].split_once(quote).map(|(name, _)| name)
}

fn json_packages(
    manifest: Option<&Value>,
    file: &str,
    sections: &[&str],
    packages: &[&str],
) -> Vec<DatabaseSignal> {
    let Some(manifest) = manifest else {
        return Vec::new();
    };
    packages
        .iter()
        .filter(|package| has_json_package(manifest, sections, package))
        .map(|package| package_signal(file, package))
        .collect()
}

fn has_json_package(manifest: &Value, sections: &[&str], package: &str) -> bool {
    sections.iter().any(|section| {
        manifest
            .get(section)
            .and_then(Value::as_object)
            .is_some_and(|dependencies| dependencies.contains_key(package))
    })
}

fn has_any_package(package_json: Option<&Value>, packages: &[&str]) -> bool {
    package_json.is_some_and(|manifest| {
        packages
            .iter()
            .any(|package| has_json_package(manifest, &PACKAGE_JSON_SECTIONS, package))
    })
}

fn package_signal(file: &str, package: &str) -> DatabaseSignal {
    DatabaseSignal {
        kind: DatabaseSignalKind::PackageDependency,
        file: file.to_owned(),
        name: Some(package.to_owned()),
    }
}

fn package_engine(package: &str) -> Option<DatabaseEngine> {
    match package {
        "pg"
        | "postgres"
        | "psycopg"
        | "psycopg2"
        | "psycopg2-binary"
        | "asyncpg"
        | "@neondatabase/serverless" => Some(DatabaseEngine::Postgresql),
        "mysql" | "mysql2" | "mysqlclient" | "pymysql" | "trilogy" => Some(DatabaseEngine::Mysql),
        "better-sqlite3" | "sqlite3" => Some(DatabaseEngine::Sqlite),
        _ => None,
    }
}

/// A short identifier taken from repository content, or `None`.
fn signal_name(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= MAX_SIGNAL_NAME_CHARS
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')))
    .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use sakala_agent_protocol::EnvExample;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::inspections::env_example;

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            std::fs::write(path, contents).expect("file");
        }
    }

    /// What an inspection of `context` reports in the two fields.
    async fn inspect(context: &Path) -> (Option<EnvExample>, DatabaseHint) {
        let env = env_example::read(context).await;
        let hint = detect(context, env.as_ref()).await;
        (env.map(|scan| scan.example), hint)
    }

    async fn inspect_files(files: &[(&str, &str)]) -> (Option<EnvExample>, DatabaseHint) {
        let temp = TempDir::new().expect("temp");
        write(temp.path(), files);
        inspect(temp.path()).await
    }

    fn signal(kind: DatabaseSignalKind, file: &str, name: Option<&str>) -> DatabaseSignal {
        DatabaseSignal {
            kind,
            file: file.to_owned(),
            name: name.map(str::to_owned),
        }
    }

    /// `laravel/` of gmedia/sakala-examples at f6e6c08 (2026-11-19): its real
    /// `.env.example`, `composer.json`, `artisan`, and migration.
    #[tokio::test]
    async fn the_laravel_example_suggests_its_variables_and_migrations() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/laravel-example");
        let (env, hint) = inspect(&fixture).await;
        let json = json!({ "env_example": env, "database_hint": hint });
        println!("{}", serde_json::to_string_pretty(&json).expect("json"));
        assert_eq!(
            json,
            json!({
                "env_example": {
                    "file": ".env.example",
                    "entries": [
                        { "key": "APP_NAME", "example_value": "Buku tamu Sakala" },
                        { "key": "APP_ENV", "example_value": "local" },
                        { "key": "APP_KEY", "example_value": null },
                        { "key": "APP_DEBUG", "example_value": "true" },
                        { "key": "APP_URL", "example_value": "http://localhost:8000" },
                        { "key": "LOG_CHANNEL", "example_value": "stack" },
                        { "key": "LOG_LEVEL", "example_value": "debug" },
                        { "key": "SESSION_DRIVER", "example_value": "cookie" },
                        { "key": "SESSION_LIFETIME", "example_value": "120" },
                        { "key": "CACHE_STORE", "example_value": "file" },
                        { "key": "QUEUE_CONNECTION", "example_value": "sync" }
                    ],
                    "truncated": false
                },
                "database_hint": {
                    "likely": true,
                    "engine": "unknown",
                    "signals": [
                        { "kind": "laravel_migrations", "file": "database/migrations", "name": null }
                    ],
                    "suggested_release_command": "php artisan migrate --force"
                }
            })
        );
    }

    #[tokio::test]
    async fn laravel_with_mysql_names_the_engine_from_db_connection() {
        let (_, hint) = inspect_files(&[
            (
                ".env.example",
                "APP_NAME=Laravel\nDB_CONNECTION=mysql\nDB_HOST=127.0.0.1\nDB_PORT=3306\nDB_DATABASE=laravel\nDB_USERNAME=root\nDB_PASSWORD=\n",
            ),
            (
                "composer.json",
                r#"{"require":{"php":"^8.3","laravel/framework":"^12.0"}}"#,
            ),
            ("artisan", "#!/usr/bin/env php\n"),
            ("database/migrations/0001_01_01_000000_create_users_table.php", "<?php\n"),
        ])
        .await;
        assert_eq!(hint.engine, DatabaseEngine::Mysql);
        assert_eq!(
            hint.signals,
            [
                signal(
                    DatabaseSignalKind::EnvVariable,
                    ".env.example",
                    Some("DB_CONNECTION")
                ),
                signal(
                    DatabaseSignalKind::EnvVariable,
                    ".env.example",
                    Some("DB_HOST")
                ),
                signal(
                    DatabaseSignalKind::LaravelMigrations,
                    "database/migrations",
                    None
                ),
            ]
        );
        assert_eq!(
            hint.suggested_release_command,
            Some(SuggestedReleaseCommand::LaravelMigrate)
        );
    }

    const PRISMA_PACKAGE_JSON: &str = r#"{
        "dependencies": { "@prisma/client": "^6.0.0", "next": "15.0.0" },
        "devDependencies": { "prisma": "^6.0.0" }
    }"#;
    const PRISMA_SCHEMA: &str = r#"
        generator client {
          provider = "prisma-client-js"
        }

        // provider = "mysql" in a comment does not count
        datasource db {
          provider = "postgresql" // the real one
          url      = env("DATABASE_URL")
        }
    "#;

    #[tokio::test]
    async fn prisma_with_migrations_suggests_migrate_deploy() {
        let (_, hint) = inspect_files(&[
            ("package.json", PRISMA_PACKAGE_JSON),
            ("prisma/schema.prisma", PRISMA_SCHEMA),
            ("prisma/migrations/20261119000000_init/migration.sql", "CREATE TABLE x();\n"),
            (".env.example", "DATABASE_URL=\"postgresql://johndoe:randompassword@localhost:5432/mydb?schema=public\"\n"),
        ])
        .await;
        assert_eq!(
            serde_json::to_value(&hint).expect("json"),
            json!({
                "likely": true,
                "engine": "postgresql",
                "signals": [
                    { "kind": "env_variable", "file": ".env.example", "name": "DATABASE_URL" },
                    { "kind": "prisma_schema", "file": "prisma/schema.prisma", "name": "postgresql" },
                    { "kind": "package_dependency", "file": "package.json", "name": "@prisma/client" },
                    { "kind": "package_dependency", "file": "package.json", "name": "prisma" }
                ],
                "suggested_release_command": "npx prisma migrate deploy"
            })
        );
    }

    #[tokio::test]
    async fn prisma_without_a_migrations_directory_suggests_no_command() {
        let (env, hint) = inspect_files(&[
            ("package.json", PRISMA_PACKAGE_JSON),
            ("prisma/schema.prisma", PRISMA_SCHEMA),
        ])
        .await;
        assert_eq!(env, None);
        assert_eq!(hint.engine, DatabaseEngine::Postgresql);
        assert!(hint.likely);
        assert_eq!(hint.suggested_release_command, None);
    }

    #[tokio::test]
    async fn django_settings_and_requirements_suggest_migrate() {
        let (_, hint) = inspect_files(&[
            ("manage.py", "#!/usr/bin/env python\n"),
            (
                "requirements.txt",
                "# web\nDjango==5.2.1\ndj-database-url>=2.3 ; python_version >= '3.10'\npsycopg[binary]==3.2.9\n-r dev.txt\n",
            ),
            (
                "mysite/settings.py",
                "import dj_database_url\nDATABASES = {'default': dj_database_url.config(default='sqlite:///db.sqlite3')}\n",
            ),
            ("mysite/__init__.py", ""),
        ])
        .await;
        assert_eq!(
            hint.signals,
            [
                signal(
                    DatabaseSignalKind::DjangoSettings,
                    "mysite/settings.py",
                    Some("dj_database_url")
                ),
                signal(
                    DatabaseSignalKind::PackageDependency,
                    "requirements.txt",
                    Some("psycopg")
                ),
                signal(
                    DatabaseSignalKind::PackageDependency,
                    "requirements.txt",
                    Some("dj-database-url")
                ),
            ]
        );
        assert_eq!(hint.engine, DatabaseEngine::Postgresql);
        assert_eq!(
            hint.suggested_release_command,
            Some(SuggestedReleaseCommand::DjangoMigrate)
        );
    }

    #[tokio::test]
    async fn django_split_settings_and_pyproject_name_the_engine() {
        let (_, hint) = inspect_files(&[
            ("manage.py", ""),
            (
                "pyproject.toml",
                "[project]\nname = \"shop\"\ndependencies = [\"django>=5\", \"mysqlclient\"]\n",
            ),
            (
                "shop/settings/base.py",
                "DATABASES = {\"default\": {\"ENGINE\": \"django.db.backends.sqlite3\"}}\n",
            ),
        ])
        .await;
        assert_eq!(
            hint.signals,
            [
                signal(
                    DatabaseSignalKind::DjangoSettings,
                    "shop/settings/base.py",
                    Some("DATABASES")
                ),
                signal(
                    DatabaseSignalKind::PackageDependency,
                    "pyproject.toml",
                    Some("mysqlclient")
                ),
            ]
        );
        // The settings ENGINE comes before package names.
        assert_eq!(hint.engine, DatabaseEngine::Sqlite);
        assert_eq!(
            hint.suggested_release_command,
            Some(SuggestedReleaseCommand::DjangoMigrate)
        );
    }

    #[tokio::test]
    async fn a_minimal_rails_app_suggests_db_migrate() {
        let (_, hint) = inspect_files(&[
            ("bin/rails", "#!/usr/bin/env ruby\n"),
            (
                "config/database.yml",
                "default: &default\n  adapter: postgresql # managed\n  pool: <%= ENV.fetch(\"RAILS_MAX_THREADS\") { 5 } %>\n",
            ),
            ("db/migrate/20261119000000_create_posts.rb", "class CreatePosts; end\n"),
            ("Gemfile", "source \"https://rubygems.org\"\ngem \"rails\", \"~> 8.0\"\ngem 'pg', '~> 1.5'\n"),
        ])
        .await;
        assert_eq!(
            hint.signals,
            [
                signal(
                    DatabaseSignalKind::RailsDatabaseConfig,
                    "config/database.yml",
                    Some("postgresql")
                ),
                signal(DatabaseSignalKind::PackageDependency, "Gemfile", Some("pg")),
            ]
        );
        assert_eq!(hint.engine, DatabaseEngine::Postgresql);
        assert_eq!(
            hint.suggested_release_command,
            Some(SuggestedReleaseCommand::RailsMigrate)
        );
    }

    #[tokio::test]
    async fn rails_with_an_erb_adapter_has_a_signal_without_a_name() {
        let (_, hint) = inspect_files(&[(
            "config/database.yml",
            "production:\n  adapter: <%= ENV['DB_ADAPTER'] %>\n",
        )])
        .await;
        assert_eq!(
            hint.signals,
            [signal(
                DatabaseSignalKind::RailsDatabaseConfig,
                "config/database.yml",
                None
            )]
        );
        assert_eq!(hint.engine, DatabaseEngine::Unknown);
        assert_eq!(hint.suggested_release_command, None);
    }

    #[tokio::test]
    async fn a_static_site_has_no_hint_and_no_env_example() {
        let (env, hint) = inspect_files(&[
            ("index.html", "<h1>hi</h1>\n"),
            ("package.json", r#"{"devDependencies":{"vite":"^8.0.0"}}"#),
        ])
        .await;
        assert_eq!(env, None);
        assert_eq!(hint, DatabaseHint::none());
    }

    #[tokio::test]
    async fn a_dot_env_file_is_never_read_for_either_field() {
        let (env, hint) = inspect_files(&[(
            ".env",
            "DB_HOST=db.internal\nDATABASE_URL=postgres://app:FAKE-SECRET@db/app\n",
        )])
        .await;
        assert_eq!(env, None);
        assert_eq!(hint, DatabaseHint::none());
        let json = serde_json::to_string(&(env, hint)).expect("json");
        assert!(!json.contains("FAKE-SECRET") && !json.contains("DB_HOST"));
    }

    #[tokio::test]
    async fn a_workspace_package_is_read_from_its_own_directory() {
        let temp = TempDir::new().expect("temp");
        write(
            temp.path(),
            &[
                (
                    ".env.example",
                    "ROOT_ONLY=1\nDATABASE_URL=mysql://root@db/app\n",
                ),
                (
                    "package.json",
                    r#"{"workspaces":["apps/*"],"dependencies":{"pg":"^8"}}"#,
                ),
                (".env.sample", "ROOT_SAMPLE=1\n"),
                ("apps/web/package.json", r#"{"name":"web"}"#),
                ("apps/web/.env.example", "PACKAGE_ONLY=1\n"),
                ("apps/api/package.json", r#"{"name":"api"}"#),
            ],
        );
        std::os::unix::fs::symlink(
            "../../.env.example",
            temp.path().join("apps/api/.env.example"),
        )
        .expect("symlink");

        let (env, hint) = inspect(&temp.path().join("apps/web")).await;
        let env = env.expect("the package's own file");
        assert_eq!(env.entries.len(), 1);
        assert_eq!(env.entries[0].key, "PACKAGE_ONLY");
        assert_eq!(hint, DatabaseHint::none());

        // A symlink to the repository root's file leaves the inspected directory.
        let (env, hint) = inspect(&temp.path().join("apps/api")).await;
        assert_eq!(env, None);
        assert_eq!(hint, DatabaseHint::none());
    }

    #[tokio::test]
    async fn signals_are_capped_with_framework_evidence_first() {
        let env: String = DATABASE_ENV_KEYS
            .iter()
            .map(|key| format!("{key}=x\n"))
            .chain(std::iter::once("DB_HOST=again\n".to_owned()))
            .collect();
        let dependencies: Vec<String> = NODE_DATABASE_PACKAGES
            .iter()
            .map(|package| format!("\"{package}\": \"1\""))
            .collect();
        let package_json = format!("{{\"dependencies\": {{{}}}}}", dependencies.join(","));
        let (_, hint) = inspect_files(&[
            (".env.example", &env),
            ("package.json", &package_json),
            ("prisma/schema.prisma", PRISMA_SCHEMA),
        ])
        .await;
        assert_eq!(hint.signals.len(), MAX_SIGNALS);
        assert_eq!(
            hint.signals
                .iter()
                .filter(|s| s.kind == DatabaseSignalKind::EnvVariable)
                .count(),
            DATABASE_ENV_KEYS.len()
        );
        assert_eq!(
            hint.signals[DATABASE_ENV_KEYS.len()].kind,
            DatabaseSignalKind::PrismaSchema
        );
        let unique: HashSet<_> = hint.signals.iter().collect();
        assert_eq!(unique.len(), hint.signals.len());
        // `DATABASE_URL=x` names no engine, so the Prisma provider decides.
        assert_eq!(hint.engine, DatabaseEngine::Postgresql);
    }

    #[test]
    fn engine_sources_follow_the_contract_order() {
        let scan = |contents: &str| {
            env_example::parse(sakala_agent_protocol::EnvExampleFile::Example, contents)
        };
        assert_eq!(
            connection_engine(&scan("DB_CONNECTION=pgsql\n")),
            Some(DatabaseEngine::Postgresql)
        );
        assert_eq!(
            connection_engine(&scan("DB_CONNECTION=mariadb\n")),
            Some(DatabaseEngine::Mysql)
        );
        assert_eq!(
            connection_engine(&scan("DB_CONNECTION=sqlite\n")),
            Some(DatabaseEngine::Sqlite)
        );
        assert_eq!(connection_engine(&scan("DB_CONNECTION=sqlsrv\n")), None);
        assert_eq!(
            url_engine(&scan(
                "DATABASE_URL=\nDB_URL=postgresql+asyncpg://u:p@h/db\n"
            )),
            Some(DatabaseEngine::Postgresql)
        );
        assert_eq!(
            url_engine(&scan("DATABASE_URL=file:./dev.db\n")),
            Some(DatabaseEngine::Sqlite)
        );
        assert_eq!(url_engine(&scan("DATABASE_URL=mongodb://h/db\n")), None);
        assert_eq!(
            django_engine("'ENGINE': 'django.db.backends.postgresql_psycopg2'"),
            Some(DatabaseEngine::Postgresql)
        );
        assert_eq!(django_engine("'ENGINE': 'django.db.backends.oracle'"), None);
        assert_eq!(
            prisma_provider("datasource db {\n  provider = \"sqlite\"\n}\n"),
            Some("sqlite")
        );
        assert_eq!(
            prisma_provider("datasource db {\n  url = env(\"X\")\n}\nprovider = \"mysql\"\n"),
            None
        );
        assert_eq!(gem_name("gem(\"mysql2\")"), Some("mysql2"));
        assert_eq!(gem_name("gemspec"), None);
        assert_eq!(
            requirement_name("Psycopg2_Binary>=2.9"),
            Some("psycopg2-binary".to_owned())
        );
        assert_eq!(signal_name("postgresql\"; rm -rf"), None);
    }
}
