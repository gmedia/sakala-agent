//! Finds the JavaScript workspace (monorepo) that a root directory belongs to.
//!
//! In a pnpm, npm, Yarn, or Bun workspace the lockfile, the workspace file, and
//! `packageManager` live at the workspace root, not in the package. Railpack run on
//! the package alone then picks the wrong package manager and has no lockfile, so
//! the build fails with `pnpm: not found` or installs other versions. Railpack does
//! support workspaces, but only when it builds from the workspace root
//! (<https://railpack.com/languages/node#monorepos-and-workspaces>), and there it
//! builds and starts the root package. So a package of a workspace is built from
//! the workspace root with commands that target the package.
//!
//! Every file read here is repository content and read on the host, so a symlink
//! is only followed while it stays inside the repository, each read is bounded, and
//! YAML aliases are refused instead of expanded. Anything unreadable means "no
//! workspace", so detection can only change a build on positive evidence.

use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use sakala_agent_protocol::{JsPackageManager, JsWorkspace};
use serde_json::Value;
use yaml_rust2::parser::{Event, Parser};

use crate::repository_file::{self, MAX_MANIFEST_BYTES, is_file_inside};
/// A workspace file lists a few patterns; this bounds the glob work it can ask for.
const MAX_WORKSPACE_PATTERNS: usize = 256;
const PACKAGE_JSON: &str = "package.json";
const PNPM_WORKSPACE: &str = "pnpm-workspace.yaml";
const PNPM_WORKSPACE_PACKAGES: &str = "packages";
/// Railpack's default output directory for Vite, and the config files it reads.
const VITE_DEFAULT_OUTPUT_DIRECTORY: &str = "dist";
const VITE_CONFIG_FILES: [&str; 2] = ["vite.config.js", "vite.config.ts"];
const VITE_OUTPUT_DIRECTORY_KEY: &str = "outDir";
const VITE_PACKAGE: &str = "vite";
const VITE_BUILD: &str = "vite build";
/// SvelteKit builds with Vite but is not a static site by default.
const SVELTEKIT_PACKAGE: &str = "@sveltejs/kit";
const BUILD_SCRIPT: &str = "build";
const START_SCRIPT: &str = "start";
/// pnpm's selector suffix for "this package and every workspace package it depends
/// on", so internal packages it imports are built first.
const PNPM_WITH_DEPENDENCIES: &str = "...";
const REPOSITORY_ROOT: &str = ".";

/// A root directory that is one package of a JavaScript workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedWorkspace {
    /// The workspace root on the host, canonical. Railpack builds from here.
    root: PathBuf,
    manager: JsPackageManager,
    root_directory: String,
    package_directory: String,
    package: PackageFacts,
}

/// What a build of the package needs from its `package.json`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PackageFacts {
    name: Option<String>,
    has_build_script: bool,
    has_start_script: bool,
    /// Relative to the package, set when it is a Vite single-page app.
    static_output_directory: Option<String>,
}

/// How Railpack builds and starts the package from the workspace root.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceCommands {
    pub build: Option<String>,
    pub start: Option<String>,
    pub static_output_directory: Option<String>,
}

impl DetectedWorkspace {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The package's directory relative to the workspace root, which is where a
    /// command of the project must run inside the image.
    #[must_use]
    pub fn package_directory(&self) -> &str {
        &self.package_directory
    }

    /// Commands the project set run in the package's directory, as they would if
    /// the package were built on its own. Without one, the package's own `build`
    /// and `start` scripts run, and a Vite single-page app without a `start`
    /// script is served as static files, as Railpack does for one at the root.
    #[must_use]
    pub fn commands(
        &self,
        build_command: Option<&str>,
        start_command: Option<&str>,
    ) -> WorkspaceCommands {
        let build = match build_command {
            Some(command) => Some(self.in_package(command)),
            None if self.package.has_build_script => Some(self.build_script()),
            None => None,
        };
        let (start, static_output_directory) = match start_command {
            Some(command) => (Some(self.in_package(command)), None),
            None if self.package.has_start_script => {
                (Some(self.in_package(&self.run_script(START_SCRIPT))), None)
            }
            None => (
                None,
                self.package
                    .static_output_directory
                    .as_ref()
                    .map(|directory| format!("{}/{directory}", self.package_directory)),
            ),
        };
        WorkspaceCommands {
            build,
            start,
            static_output_directory,
        }
    }

    /// What the API and the Console show, with the commands a build would use.
    #[must_use]
    pub fn summary(&self, commands: &WorkspaceCommands) -> JsWorkspace {
        JsWorkspace {
            manager: self.manager,
            root_directory: self.root_directory.clone(),
            package_directory: self.package_directory.clone(),
            package_name: self.package.name.clone(),
            build_command: commands.build.clone(),
            start_command: commands.start.clone(),
            static_output_directory: commands.static_output_directory.clone(),
        }
    }

    /// Prefixes a command so it runs in the package's directory. The image keeps
    /// the workspace root as its working directory.
    #[must_use]
    pub fn in_package(&self, command: &str) -> String {
        in_directory(&self.package_directory, command)
    }

    fn run_script(&self, script: &str) -> String {
        format!("{} run {script}", self.manager.as_str())
    }

    /// pnpm can build the package together with the workspace packages it depends
    /// on, in dependency order. The other managers have no such selector, so their
    /// script runs in the package alone.
    fn build_script(&self) -> String {
        match (&self.manager, &self.package.name) {
            (JsPackageManager::Pnpm, Some(name)) => format!(
                "pnpm --filter {} run {BUILD_SCRIPT}",
                shell_quote(&format!("{name}{PNPM_WITH_DEPENDENCIES}"))
            ),
            _ => self.in_package(&self.run_script(BUILD_SCRIPT)),
        }
    }
}

/// `command` run in `directory`, relative to the image's working directory.
#[must_use]
pub fn in_directory(directory: &str, command: &str) -> String {
    format!("cd {} && {command}", shell_quote(directory))
}

/// Railpack runs its commands with `bash -c`, so every value from the repository
/// is quoted.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Looks for the nearest directory above `package`, up to `repository`, that is a
/// workspace root, and returns it when its workspace includes `package`. Both paths
/// must be canonical, as `DeploymentWorkspace::build_context` returns them.
pub async fn detect(repository: &Path, package: &Path) -> Option<DetectedWorkspace> {
    let relative = package.strip_prefix(repository).ok()?;
    let components = normal_components(relative)?;
    if components.is_empty() {
        return None;
    }
    let package_json = read_json(repository, &package.join(PACKAGE_JSON)).await?;
    let package_facts = package_facts(repository, package, &package_json).await;
    for depth in (0..components.len()).rev() {
        let root = components[..depth]
            .iter()
            .fold(repository.to_path_buf(), |path, part| path.join(part));
        let Some(root_package_json) = read_json(repository, &root.join(PACKAGE_JSON)).await else {
            continue;
        };
        let Some(patterns) = workspace_patterns(repository, &root, &root_package_json).await else {
            continue;
        };
        // The nearest workspace root decides, as a package manager would.
        let package_directory = components[depth..].join("/");
        if !is_member(&patterns, &package_directory) {
            return None;
        }
        return Some(DetectedWorkspace {
            manager: package_manager(repository, &root, &root_package_json).await,
            root,
            root_directory: if depth == 0 {
                REPOSITORY_ROOT.to_owned()
            } else {
                components[..depth].join("/")
            },
            package_directory,
            package: package_facts,
        });
    }
    None
}

fn normal_components(relative: &Path) -> Option<Vec<String>> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Railpack's order: `pnpm-workspace.yaml` when it lists packages, otherwise the
/// `workspaces` field of `package.json`, as an array or as `{ "packages": [...] }`.
async fn workspace_patterns(
    repository: &Path,
    root: &Path,
    package_json: &Value,
) -> Option<Vec<String>> {
    if let Some(yaml) = read_text(repository, &root.join(PNPM_WORKSPACE)).await
        && let Some(patterns) = pnpm_workspace_packages(&yaml).filter(|list| !list.is_empty())
    {
        return Some(patterns);
    }
    let workspaces = package_json.get("workspaces")?;
    let list = workspaces
        .as_array()
        .or_else(|| workspaces.get("packages")?.as_array())?;
    let patterns = list
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (!patterns.is_empty()).then_some(patterns)
}

/// The `packages` list at the top of `pnpm-workspace.yaml`. Read from parser
/// events, because the document loader copies a node for every alias, which lets a
/// few hundred bytes ask for gigabytes. A file with an alias is not read at all.
fn pnpm_workspace_packages(yaml: &str) -> Option<Vec<String>> {
    let mut parser = Parser::new_from_str(yaml);
    let mut stack: Vec<YamlFrame> = Vec::new();
    let mut packages: Option<Vec<String>> = None;
    loop {
        let (event, _) = parser.next_token().ok()?;
        match event {
            Event::StreamEnd => return packages,
            Event::Alias(_) => return None,
            Event::Scalar(value, ..) => match stack.as_mut_slice() {
                [
                    YamlFrame::Mapping {
                        expecting_key: expecting_key @ true,
                        key_is_packages,
                    },
                ] => {
                    *key_is_packages = value == PNPM_WORKSPACE_PACKAGES;
                    *expecting_key = false;
                }
                [_, YamlFrame::Sequence { is_packages: true }] => packages.as_mut()?.push(value),
                frames => {
                    enter_node(frames);
                }
            },
            Event::SequenceStart(..) => {
                let is_packages = enter_node(&mut stack);
                if is_packages {
                    packages = Some(Vec::new());
                }
                stack.push(YamlFrame::Sequence { is_packages });
            }
            Event::MappingStart(..) => {
                enter_node(&mut stack);
                stack.push(YamlFrame::Mapping {
                    expecting_key: true,
                    key_is_packages: false,
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                stack.pop()?;
            }
            Event::Nothing | Event::StreamStart | Event::DocumentStart | Event::DocumentEnd => {}
        }
    }
}

enum YamlFrame {
    Mapping {
        expecting_key: bool,
        key_is_packages: bool,
    },
    Sequence {
        is_packages: bool,
    },
}

/// Moves the enclosing mapping between key and value, and tells whether the node
/// is the value of the top-level `packages` key.
fn enter_node(stack: &mut [YamlFrame]) -> bool {
    let top_level = stack.len() == 1;
    let Some(YamlFrame::Mapping {
        expecting_key,
        key_is_packages,
    }) = stack.last_mut()
    else {
        return false;
    };
    let is_key = *expecting_key;
    *expecting_key = !is_key;
    if is_key {
        *key_is_packages = false;
        false
    } else {
        top_level && *key_is_packages
    }
}

/// pnpm and npm match a package's directory against the patterns, and a pattern
/// starting with `!` excludes what it matches.
fn is_member(patterns: &[String], package_directory: &str) -> bool {
    let mut included = false;
    for pattern in patterns.iter().take(MAX_WORKSPACE_PATTERNS) {
        let (negated, pattern) = match pattern.trim().strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.trim()),
        };
        let Some(matcher) = directory_matcher(pattern) else {
            continue;
        };
        if matcher.is_match(package_directory) {
            if negated {
                return false;
            }
            included = true;
        }
    }
    included
}

fn directory_matcher(pattern: &str) -> Option<GlobMatcher> {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() {
        return None;
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

/// Railpack's order, so the command uses the manager Railpack installs:
/// `packageManager`, then lockfiles, then `engines`, then npm.
async fn package_manager(repository: &Path, root: &Path, package_json: &Value) -> JsPackageManager {
    if let Some(field) = package_json.get("packageManager").and_then(Value::as_str) {
        let name = field.split('@').next().unwrap_or_default().trim();
        if let Some(manager) = manager_named(name) {
            return manager;
        }
    }
    for (file, manager) in [
        ("pnpm-lock.yaml", JsPackageManager::Pnpm),
        ("bun.lockb", JsPackageManager::Bun),
        ("bun.lock", JsPackageManager::Bun),
        (".yarnrc.yml", JsPackageManager::Yarn),
        (".yarnrc.yaml", JsPackageManager::Yarn),
        ("yarn.lock", JsPackageManager::Yarn),
    ] {
        if is_file_inside(repository, &root.join(file)).await {
            return manager;
        }
    }
    if let Some(engines) = package_json.get("engines") {
        for name in ["pnpm", "bun", "yarn"] {
            if engines
                .get(name)
                .and_then(Value::as_str)
                .is_some_and(|version| !version.trim().is_empty())
                && let Some(manager) = manager_named(name)
            {
                return manager;
            }
        }
    }
    JsPackageManager::Npm
}

fn manager_named(name: &str) -> Option<JsPackageManager> {
    match name {
        "npm" => Some(JsPackageManager::Npm),
        "pnpm" => Some(JsPackageManager::Pnpm),
        "yarn" => Some(JsPackageManager::Yarn),
        "bun" => Some(JsPackageManager::Bun),
        _ => None,
    }
}

async fn package_facts(repository: &Path, package: &Path, package_json: &Value) -> PackageFacts {
    let script = |name: &str| {
        package_json
            .get("scripts")
            .and_then(|scripts| scripts.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|script| !script.is_empty())
    };
    let build_script = script(BUILD_SCRIPT);
    let static_output_directory = match build_script {
        Some(build) if is_vite_app(repository, package, package_json, build).await => {
            Some(vite_output_directory(repository, package).await)
        }
        _ => None,
    };
    PackageFacts {
        name: package_json
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        has_build_script: build_script.is_some(),
        has_start_script: script(START_SCRIPT).is_some(),
        static_output_directory,
    }
}

/// Railpack's rule for a Vite single-page app: `vite` is a dependency, there is a
/// `build` script, and that script runs `vite build` or a Vite config exists. A
/// SvelteKit app is not one.
async fn is_vite_app(repository: &Path, package: &Path, package_json: &Value, build: &str) -> bool {
    if !has_dependency(package_json, VITE_PACKAGE)
        || has_dependency(package_json, SVELTEKIT_PACKAGE)
    {
        return false;
    }
    if build.to_lowercase().contains(VITE_BUILD) {
        return true;
    }
    for config in VITE_CONFIG_FILES {
        if is_file_inside(repository, &package.join(config)).await {
            return true;
        }
    }
    false
}

fn has_dependency(package_json: &Value, name: &str) -> bool {
    ["dependencies", "devDependencies"].iter().any(|field| {
        package_json
            .get(field)
            .is_some_and(|dependencies| dependencies.get(name).is_some())
    })
}

/// `build.outDir` from the Vite config, read the way Railpack reads it, or `dist`.
/// A value that would leave the package is ignored.
async fn vite_output_directory(repository: &Path, package: &Path) -> String {
    for config in VITE_CONFIG_FILES {
        if let Some(contents) = read_text(repository, &package.join(config)).await {
            if let Some(directory) = quoted_value_after(&contents, VITE_OUTPUT_DIRECTORY_KEY)
                .and_then(|directory| descending_path(&directory))
            {
                return directory;
            }
            break;
        }
    }
    VITE_DEFAULT_OUTPUT_DIRECTORY.to_owned()
}

/// The first `key: 'value'` or `key: "value"` in `contents`.
fn quoted_value_after(contents: &str, key: &str) -> Option<String> {
    let mut rest = contents;
    while let Some(at) = rest.find(key) {
        rest = &rest[at + key.len()..];
        let Some(after_colon) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let value = after_colon.trim_start();
        let Some(quote) = value.chars().next().filter(|c| *c == '\'' || *c == '"') else {
            continue;
        };
        let value = &value[quote.len_utf8()..];
        let end = value.find(quote)?;
        return Some(value[..end].to_owned());
    }
    None
}

fn descending_path(value: &str) -> Option<String> {
    let value = value
        .strip_prefix("./")
        .unwrap_or(value)
        .trim_end_matches('/');
    let parts = normal_components(Path::new(value))?;
    (!parts.is_empty() && !value.chars().any(char::is_control)).then(|| parts.join("/"))
}

async fn read_json(repository: &Path, path: &Path) -> Option<Value> {
    serde_json::from_str(&read_text(repository, path).await?).ok()
}

/// A file inside the repository, at most `MAX_MANIFEST_BYTES`.
async fn read_text(repository: &Path, path: &Path) -> Option<String> {
    repository_file::read_text(repository, path, MAX_MANIFEST_BYTES).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;
    use tokio::fs;

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            std::fs::write(path, contents).expect("file");
        }
    }

    async fn detect_in(files: &[(&str, &str)], package: &str) -> Option<DetectedWorkspace> {
        let temp = TempDir::new().expect("temp directory");
        write(temp.path(), files);
        let repository = fs::canonicalize(temp.path()).await.expect("repository");
        let package = fs::canonicalize(repository.join(package))
            .await
            .expect("package");
        detect(&repository, &package).await
    }

    const VITE_APP: &str = r#"{
        "name": "@demo/web",
        "scripts": { "build": "tsc -b && vite build", "preview": "vite preview" },
        "devDependencies": { "vite": "^8.0.0" }
    }"#;
    const NODE_API: &str = r#"{
        "name": "@demo/api",
        "scripts": { "build": "tsc", "start": "node dist/server.js" }
    }"#;

    #[tokio::test]
    async fn a_pnpm_package_builds_with_its_dependencies_and_serves_its_vite_output() {
        let workspace = detect_in(
            &[
                (
                    "package.json",
                    r#"{"name":"demo","packageManager":"pnpm@10.0.0"}"#,
                ),
                (
                    "pnpm-workspace.yaml",
                    "packages:\n  - apps/*\n  - 'packages/*'\n",
                ),
                ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
                ("apps/web/package.json", VITE_APP),
            ],
            "apps/web",
        )
        .await
        .expect("a pnpm workspace package");

        let commands = workspace.commands(None, None);
        assert_eq!(
            workspace.summary(&commands),
            JsWorkspace {
                manager: JsPackageManager::Pnpm,
                root_directory: ".".to_owned(),
                package_directory: "apps/web".to_owned(),
                package_name: Some("@demo/web".to_owned()),
                build_command: Some("pnpm --filter '@demo/web...' run build".to_owned()),
                start_command: None,
                static_output_directory: Some("apps/web/dist".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn a_package_with_a_start_script_starts_it_in_its_own_directory() {
        let workspace = detect_in(
            &[
                ("package.json", r#"{"name":"demo"}"#),
                ("pnpm-workspace.yaml", "packages: [apps/*]\n"),
                ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
                ("apps/api/package.json", NODE_API),
            ],
            "apps/api",
        )
        .await
        .expect("a pnpm workspace package");

        let commands = workspace.commands(None, None);
        assert_eq!(
            commands.start.as_deref(),
            Some("cd 'apps/api' && pnpm run start")
        );
        assert_eq!(commands.static_output_directory, None);
    }

    #[tokio::test]
    async fn the_projects_commands_run_in_the_package_directory() {
        let workspace = detect_in(
            &[
                ("package.json", r#"{"name":"demo"}"#),
                ("pnpm-workspace.yaml", "packages: [apps/*]\n"),
                ("apps/web/package.json", VITE_APP),
            ],
            "apps/web",
        )
        .await
        .expect("a pnpm workspace package");

        let commands = workspace.commands(Some("pnpm run build"), Some("pnpm run preview"));
        assert_eq!(
            commands,
            WorkspaceCommands {
                build: Some("cd 'apps/web' && pnpm run build".to_owned()),
                start: Some("cd 'apps/web' && pnpm run preview".to_owned()),
                static_output_directory: None,
            }
        );
    }

    #[tokio::test]
    async fn npm_workspaces_run_the_script_in_the_package() {
        let workspace = detect_in(
            &[
                (
                    "package.json",
                    r#"{"name":"demo","workspaces":["packages/*"]}"#,
                ),
                ("package-lock.json", "{}"),
                ("packages/api/package.json", NODE_API),
            ],
            "packages/api",
        )
        .await
        .expect("an npm workspace package");

        assert_eq!(workspace.manager, JsPackageManager::Npm);
        assert_eq!(
            workspace.commands(None, None),
            WorkspaceCommands {
                build: Some("cd 'packages/api' && npm run build".to_owned()),
                start: Some("cd 'packages/api' && npm run start".to_owned()),
                static_output_directory: None,
            }
        );
    }

    #[tokio::test]
    async fn yarn_workspaces_in_the_object_form_are_found() {
        let workspace = detect_in(
            &[
                (
                    "package.json",
                    r#"{"name":"demo","private":true,"workspaces":{"packages":["apps/**"],"nohoist":[]}}"#,
                ),
                ("yarn.lock", "# yarn lockfile v1\n"),
                ("apps/group/web/package.json", VITE_APP),
                ("apps/group/web/vite.config.ts", "export default { build: { outDir: \"build/client\" } }\n"),
            ],
            "apps/group/web",
        )
        .await
        .expect("a yarn workspace package");

        assert_eq!(workspace.manager, JsPackageManager::Yarn);
        assert_eq!(
            workspace.commands(None, None),
            WorkspaceCommands {
                build: Some("cd 'apps/group/web' && yarn run build".to_owned()),
                start: None,
                static_output_directory: Some("apps/group/web/build/client".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn the_nearest_workspace_root_is_the_build_context() {
        let workspace = detect_in(
            &[
                ("README.md", "not a JavaScript repository at the root\n"),
                (
                    "frontend/package.json",
                    r#"{"name":"front","packageManager":"pnpm@10.0.0"}"#,
                ),
                (
                    "frontend/pnpm-workspace.yaml",
                    "packages:\n  - \"apps/*\"\n",
                ),
                ("frontend/apps/web/package.json", VITE_APP),
            ],
            "frontend/apps/web",
        )
        .await
        .expect("a nested workspace package");

        assert_eq!(workspace.root_directory, "frontend");
        assert_eq!(workspace.package_directory(), "apps/web");
        assert!(workspace.root().ends_with("frontend"));
    }

    #[tokio::test]
    async fn a_repository_without_a_workspace_is_left_alone() {
        for (files, package) in [
            // A plain repository with a subfolder app that has its own lockfile.
            (
                vec![
                    ("package.json", r#"{"name":"site"}"#),
                    ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
                    ("apps/web/package.json", VITE_APP),
                ],
                "apps/web",
            ),
            // A workspace that does not list the package.
            (
                vec![
                    ("package.json", r#"{"name":"demo"}"#),
                    ("pnpm-workspace.yaml", "packages: [packages/*]\n"),
                    ("apps/web/package.json", VITE_APP),
                ],
                "apps/web",
            ),
            // A package that a negated pattern excludes.
            (
                vec![
                    ("package.json", r#"{"name":"demo"}"#),
                    (
                        "pnpm-workspace.yaml",
                        "packages: ['apps/**', '!apps/legacy']\n",
                    ),
                    ("apps/legacy/package.json", NODE_API),
                ],
                "apps/legacy",
            ),
            // A PHP app next to the workspace, as in a Laravel API plus a Vite web.
            (
                vec![
                    ("package.json", r#"{"name":"demo"}"#),
                    ("pnpm-workspace.yaml", "packages: [apps/web]\n"),
                    ("apps/api/composer.json", "{}"),
                ],
                "apps/api",
            ),
            // `*` stays within one directory.
            (
                vec![
                    ("package.json", r#"{"name":"demo","workspaces":["apps/*"]}"#),
                    ("apps/group/web/package.json", VITE_APP),
                ],
                "apps/group/web",
            ),
        ] {
            assert_eq!(detect_in(&files, package).await, None, "{files:?}");
        }
    }

    #[tokio::test]
    async fn the_repository_root_itself_is_never_a_workspace_package() {
        assert_eq!(
            detect_in(
                &[
                    ("package.json", r#"{"name":"demo","workspaces":["apps/*"]}"#),
                    ("apps/web/package.json", VITE_APP),
                ],
                ".",
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn a_symlinked_manifest_outside_the_repository_is_not_read() {
        let temp = TempDir::new().expect("temp directory");
        let outside = TempDir::new().expect("outside directory");
        write(
            outside.path(),
            &[("package.json", r#"{"name":"host","workspaces":["apps/*"]}"#)],
        );
        write(temp.path(), &[("apps/web/package.json", VITE_APP)]);
        std::os::unix::fs::symlink(
            outside.path().join("package.json"),
            temp.path().join("package.json"),
        )
        .expect("symlink");
        let repository = fs::canonicalize(temp.path()).await.expect("repository");
        let package = repository.join("apps/web");
        assert_eq!(detect(&repository, &package).await, None);
    }

    #[test]
    fn pnpm_workspace_packages_reads_the_forms_pnpm_accepts() {
        for (yaml, expected) in [
            (
                "packages:\n  - apps/*\n  - '!**/test/**'\n",
                Some(vec!["apps/*", "!**/test/**"]),
            ),
            (
                "# comment\npackages: [\"apps/*\", packages/*]\n",
                Some(vec!["apps/*", "packages/*"]),
            ),
            (
                "catalog:\n  react: ^19\npackages:\n  - apps/*\nonlyBuiltDependencies:\n  - esbuild\n",
                Some(vec!["apps/*"]),
            ),
            ("catalog:\n  packages: [nested/*]\n", None),
            ("onlyBuiltDependencies: [esbuild]\n", None),
            ("", None),
            ("packages: [", None),
        ] {
            assert_eq!(
                pnpm_workspace_packages(yaml),
                expected.map(|list| list.into_iter().map(str::to_owned).collect()),
                "{yaml:?}"
            );
        }
    }

    #[test]
    fn a_yaml_alias_is_refused_instead_of_expanded() {
        let laughs = "a: &a [x, x, x, x, x, x, x, x, x]\nb: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a]\npackages: [*b]\n";
        assert_eq!(pnpm_workspace_packages(laughs), None);
    }

    #[test]
    fn workspace_patterns_match_like_pnpm_and_npm() {
        let patterns = |list: &[&str]| list.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        assert!(is_member(&patterns(&["apps/*"]), "apps/web"));
        assert!(is_member(&patterns(&["./apps/web/"]), "apps/web"));
        assert!(is_member(&patterns(&["apps/**"]), "apps/group/web"));
        assert!(is_member(&patterns(&["{apps,services}/*"]), "services/api"));
        assert!(!is_member(&patterns(&["apps/*"]), "apps/group/web"));
        assert!(!is_member(&patterns(&["apps/*", "!apps/web"]), "apps/web"));
        assert!(!is_member(&patterns(&["["]), "apps/web"));
    }

    #[test]
    fn a_vite_output_directory_is_read_like_railpack_and_stays_in_the_package() {
        assert_eq!(
            quoted_value_after("build: { outDir : 'out' }", "outDir").as_deref(),
            Some("out")
        );
        assert_eq!(
            quoted_value_after(
                "// outDir is set below\nbuild: { outDir: \"web\" }",
                "outDir"
            )
            .as_deref(),
            Some("web")
        );
        assert_eq!(quoted_value_after("build: { outDir }", "outDir"), None);
        assert_eq!(
            descending_path("./build/client/").as_deref(),
            Some("build/client")
        );
        assert_eq!(descending_path("../outside"), None);
        assert_eq!(descending_path("/srv/www"), None);
    }

    #[test]
    fn a_directory_or_name_cannot_break_out_of_its_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
    }
}
