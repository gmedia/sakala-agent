use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RepositoryAccess;

/// Immutable repository input used for the lightweight project-preview workflow.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InspectProjectPayload {
    pub repository_url: String,
    pub commit_sha: String,
    #[serde(default)]
    pub repository_access: RepositoryAccess,
    /// Inspects this directory instead of the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_directory: Option<String>,
}

/// Stable Sakala metadata plus the raw Railpack result for forward compatibility.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProjectInspection {
    pub repository_url: String,
    pub commit_sha: String,
    pub dockerfile_found: bool,
    pub env_example_found: bool,
    pub compose_found: bool,
    pub manifests: Vec<String>,
    pub package_manager: Option<String>,
    /// TCP ports that the final stage of `Dockerfile` declares with `EXPOSE`, sorted
    /// and without duplicates. Empty without a Dockerfile, without `EXPOSE`, or when no
    /// port could be resolved. Agents before this field omitted it.
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    /// The JavaScript workspace the root directory belongs to, when it is a package
    /// of one at a higher directory. Always sent, `null` when there is none, so a
    /// missing field means an Agent before this field.
    #[serde(default)]
    pub workspace: Option<JsWorkspace>,
    /// The variables listed by the first environment example file in the inspected
    /// directory. Always sent, `null` when there is none or it cannot be read safely,
    /// so a missing field means an Agent before this field.
    #[serde(default)]
    pub env_example: Option<EnvExample>,
    /// Whether the project looks like it needs a database. Always sent by an Agent
    /// that knows it; a missing field (read as `None`) means an Agent before this field.
    #[serde(default)]
    pub database_hint: Option<DatabaseHint>,
    pub railpack: Value,
}

/// The environment example file names an Agent reads, in the order it looks for
/// them. No other name, `.env` included, is ever opened.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EnvExampleFile {
    #[serde(rename = ".env.example")]
    Example,
    #[serde(rename = ".env.sample")]
    Sample,
    #[serde(rename = ".env.dist")]
    Dist,
}

impl EnvExampleFile {
    /// Every candidate, first one that exists wins.
    pub const CANDIDATES: [Self; 3] = [Self::Example, Self::Sample, Self::Dist];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Example => ".env.example",
            Self::Sample => ".env.sample",
            Self::Dist => ".env.dist",
        }
    }
}

/// The variables of an environment example file, in file order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvExample {
    pub file: EnvExampleFile,
    pub entries: Vec<EnvExampleEntry>,
    /// `true` when the file listed more keys than the entry limit.
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvExampleEntry {
    pub key: String,
    /// `null` when the value is empty, spans lines, or looks like a credential.
    pub example_value: Option<String>,
}

/// Evidence that a project uses a database, and what to run before it starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DatabaseHint {
    /// `true` exactly when `signals` is not empty.
    pub likely: bool,
    pub engine: DatabaseEngine,
    pub signals: Vec<DatabaseSignal>,
    pub suggested_release_command: Option<SuggestedReleaseCommand>,
}

impl DatabaseHint {
    /// The hint for a project without any database evidence.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            likely: false,
            engine: DatabaseEngine::Unknown,
            signals: Vec::new(),
            suggested_release_command: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseEngine {
    Postgresql,
    Mysql,
    Sqlite,
    Unknown,
}

/// One piece of evidence. `file` is relative to the inspected directory.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct DatabaseSignal {
    pub kind: DatabaseSignalKind,
    pub file: String,
    pub name: Option<String>,
}

/// Stable signal kinds. New kinds may be added; consumers ignore unknown ones.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseSignalKind {
    EnvVariable,
    LaravelMigrations,
    PrismaSchema,
    DjangoSettings,
    RailsDatabaseConfig,
    PackageDependency,
}

/// The only release commands an Agent suggests, each for a framework it detected
/// with its migrations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SuggestedReleaseCommand {
    #[serde(rename = "php artisan migrate --force")]
    LaravelMigrate,
    #[serde(rename = "python manage.py migrate")]
    DjangoMigrate,
    #[serde(rename = "npx prisma migrate deploy")]
    PrismaMigrateDeploy,
    #[serde(rename = "bin/rails db:migrate")]
    RailsMigrate,
}

impl SuggestedReleaseCommand {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaravelMigrate => "php artisan migrate --force",
            Self::DjangoMigrate => "python manage.py migrate",
            Self::PrismaMigrateDeploy => "npx prisma migrate deploy",
            Self::RailsMigrate => "bin/rails db:migrate",
        }
    }
}

/// The package manager that installs a JavaScript workspace, as Railpack picks it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JsPackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl JsPackageManager {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }
}

/// A root directory that is one package of a JavaScript workspace (monorepo). Its
/// lockfile and package manager live at the workspace root, so a Railpack build
/// runs there and the commands below target the package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JsWorkspace {
    pub manager: JsPackageManager,
    /// The workspace root, relative to the repository root, `.` for the repository
    /// root itself. The build context.
    pub root_directory: String,
    /// The package, relative to the workspace root.
    pub package_directory: String,
    /// `name` from the package's `package.json`, `null` when it has none.
    pub package_name: Option<String>,
    /// The build command that runs at the workspace root, `null` when none is set
    /// and the package has no `build` script.
    pub build_command: Option<String>,
    /// The start command, `null` when the image serves `static_output_directory`
    /// or when neither a command nor a `start` script exists.
    pub start_command: Option<String>,
    /// The directory of a static build (a Vite single-page app), relative to the
    /// workspace root, which the image serves with Railpack's own web server.
    pub static_output_directory: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        DatabaseEngine, DatabaseHint, DatabaseSignal, DatabaseSignalKind, EnvExample,
        EnvExampleEntry, EnvExampleFile, JsPackageManager, JsWorkspace, ProjectInspection,
        SuggestedReleaseCommand,
    };

    fn result_without_ports() -> serde_json::Value {
        // A result from an Agent before `exposed_ports` and `workspace`.
        json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "dockerfile_found": true,
            "env_example_found": false,
            "compose_found": false,
            "manifests": ["Dockerfile"],
            "package_manager": null,
            "railpack": {}
        })
    }

    #[test]
    fn a_result_from_an_agent_before_exposed_ports_still_reads() {
        let inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert!(inspection.exposed_ports.is_empty());
    }

    #[test]
    fn exposed_ports_is_always_sent_so_absence_means_an_older_agent() {
        let mut inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["exposed_ports"],
            json!([])
        );
        inspection.exposed_ports = vec![80];
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["exposed_ports"],
            json!([80])
        );
    }

    #[test]
    fn workspace_is_null_without_one_and_absent_only_from_an_older_agent() {
        let mut inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert_eq!(inspection.workspace, None);
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["workspace"],
            serde_json::Value::Null
        );
        inspection.workspace = Some(JsWorkspace {
            manager: JsPackageManager::Pnpm,
            root_directory: ".".to_owned(),
            package_directory: "apps/web".to_owned(),
            package_name: Some("@example/web".to_owned()),
            build_command: Some("pnpm --filter '@example/web...' run build".to_owned()),
            start_command: None,
            static_output_directory: Some("apps/web/dist".to_owned()),
        });
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["workspace"],
            json!({
                "manager": "pnpm",
                "root_directory": ".",
                "package_directory": "apps/web",
                "package_name": "@example/web",
                "build_command": "pnpm --filter '@example/web...' run build",
                "start_command": null,
                "static_output_directory": "apps/web/dist"
            })
        );
    }

    #[test]
    fn env_example_and_database_hint_are_absent_only_from_an_older_agent() {
        let inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert_eq!(inspection.env_example, None);
        assert_eq!(inspection.database_hint, None);
        let json = serde_json::to_value(&inspection).expect("json");
        assert_eq!(json["env_example"], serde_json::Value::Null);
        assert!(
            json.as_object()
                .expect("object")
                .contains_key("env_example")
        );
        assert!(
            json.as_object()
                .expect("object")
                .contains_key("database_hint")
        );
    }

    #[test]
    fn env_example_and_database_hint_serialize_with_the_contract_shape() {
        let mut inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        inspection.env_example = Some(EnvExample {
            file: EnvExampleFile::Example,
            entries: vec![
                EnvExampleEntry {
                    key: "APP_NAME".to_owned(),
                    example_value: Some("Buku tamu Sakala".to_owned()),
                },
                EnvExampleEntry {
                    key: "APP_KEY".to_owned(),
                    example_value: None,
                },
            ],
            truncated: false,
        });
        inspection.database_hint = Some(DatabaseHint {
            likely: true,
            engine: DatabaseEngine::Postgresql,
            signals: vec![DatabaseSignal {
                kind: DatabaseSignalKind::PrismaSchema,
                file: "prisma/schema.prisma".to_owned(),
                name: Some("postgresql".to_owned()),
            }],
            suggested_release_command: Some(SuggestedReleaseCommand::PrismaMigrateDeploy),
        });
        let json = serde_json::to_value(&inspection).expect("json");
        assert_eq!(
            json["env_example"],
            json!({
                "file": ".env.example",
                "entries": [
                    { "key": "APP_NAME", "example_value": "Buku tamu Sakala" },
                    { "key": "APP_KEY", "example_value": null }
                ],
                "truncated": false
            })
        );
        assert_eq!(
            json["database_hint"],
            json!({
                "likely": true,
                "engine": "postgresql",
                "signals": [
                    { "kind": "prisma_schema", "file": "prisma/schema.prisma", "name": "postgresql" }
                ],
                "suggested_release_command": "npx prisma migrate deploy"
            })
        );
        let read_back: ProjectInspection = serde_json::from_value(json).expect("round trip");
        assert_eq!(read_back, inspection);
    }

    #[test]
    fn a_hint_without_evidence_is_an_explicit_object() {
        assert_eq!(
            serde_json::to_value(DatabaseHint::none()).expect("json"),
            json!({
                "likely": false,
                "engine": "unknown",
                "signals": [],
                "suggested_release_command": null
            })
        );
    }

    #[test]
    fn wire_names_match_the_string_helpers() {
        for file in EnvExampleFile::CANDIDATES {
            assert_eq!(serde_json::to_value(file).expect("json"), file.as_str());
        }
        for command in [
            SuggestedReleaseCommand::LaravelMigrate,
            SuggestedReleaseCommand::DjangoMigrate,
            SuggestedReleaseCommand::PrismaMigrateDeploy,
            SuggestedReleaseCommand::RailsMigrate,
        ] {
            assert_eq!(
                serde_json::to_value(command).expect("json"),
                command.as_str()
            );
        }
    }
}
