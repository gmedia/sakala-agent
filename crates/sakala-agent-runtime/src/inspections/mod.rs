mod base_image;
mod dockerfile;

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::ProjectInspection;
use serde_json::Value;
use tokio::{fs, io::AsyncReadExt};

use self::base_image::{PublicImage, RegistryLookup};
use crate::{
    ProcessRunner, RuntimeError, RuntimeReporter, railpack::RailpackSandbox,
    workspace::RepositorySource,
};

#[async_trait]
pub trait ProjectInspector: Send + Sync {
    async fn inspect(
        &self,
        context: &Path,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectInspection, RuntimeError>;
}

pub struct RailpackProjectInspector {
    sandbox: Arc<RailpackSandbox>,
    registry: RegistryLookup,
}

impl RailpackProjectInspector {
    #[must_use]
    pub fn new(sandbox: Arc<RailpackSandbox>, runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            sandbox,
            registry: RegistryLookup::new(runner),
        }
    }
}

#[async_trait]
impl ProjectInspector for RailpackProjectInspector {
    async fn inspect(
        &self,
        context: &Path,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectInspection, RuntimeError> {
        let manifests = detected_manifests(context).await?;
        let dockerfile_found = manifests.iter().any(|name| name == "Dockerfile");
        let railpack: Value = match self.sandbox.info(context, reporter).await {
            // The parse error is not reported: the report is attacker-controlled.
            Ok(info) => serde_json::from_slice(&info).map_err(|_| {
                RuntimeError::Execution("Railpack produced an unreadable report".to_owned())
            })?,
            // Railpack fails on a repository with only a Dockerfile, but a Dockerfile
            // build never asks it. Its output is already in the command log.
            Err(RuntimeError::Build(_)) if dockerfile_found => Value::Null,
            Err(error) => return Err(error),
        };
        let exposed_ports = match read_dockerfile(context).await {
            Some(contents) => exposed_ports(&contents, &self.registry).await,
            None => Vec::new(),
        };

        Ok(ProjectInspection {
            repository_url: source.repository_url.clone(),
            commit_sha: source.commit_sha.clone(),
            dockerfile_found,
            env_example_found: manifests.iter().any(|name| name == ".env.example"),
            compose_found: manifests.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "compose.yml" | "compose.yaml" | "docker-compose.yml" | "docker-compose.yaml"
                )
            }),
            package_manager: detect_package_manager(&manifests).map(str::to_owned),
            manifests,
            exposed_ports,
            railpack,
        })
    }
}

/// Ports from `EXPOSE` in the final stage. Without any `EXPOSE` there, the ports
/// come from the config of the public image that stage is built on, read from the
/// registry with at most this one lookup per inspection.
async fn exposed_ports(contents: &str, registry: &RegistryLookup) -> Vec<u16> {
    const MAX_PORTS: usize = 32;
    let dockerfile = dockerfile::read(contents);
    let Some(base_image) = dockerfile.base_image else {
        return dockerfile.ports;
    };
    let Some(image) = PublicImage::parse(&base_image) else {
        // The reference is repository content, so it is not logged.
        tracing::debug!("base image is not on a public registry; lookup skipped");
        return dockerfile.ports;
    };
    let mut ports: Vec<u16> = dockerfile.ports;
    ports.extend(registry.exposed_ports(&image).await);
    ports.sort_unstable();
    ports.dedup();
    ports.truncate(MAX_PORTS);
    ports
}

/// Larger than any real Dockerfile; the file comes from the repository.
const MAX_DOCKERFILE_BYTES: u64 = 1024 * 1024;

/// The Dockerfile a build in `context` would use, or `None` when there is none to read.
/// A symlink is followed only while it stays inside the context, because the
/// repository chooses where it points.
async fn read_dockerfile(context: &Path) -> Option<String> {
    let root = fs::canonicalize(context).await.ok()?;
    let path = fs::canonicalize(context.join("Dockerfile")).await.ok()?;
    if !path.starts_with(&root) || !fs::metadata(&path).await.ok()?.is_file() {
        return None;
    }
    let mut contents = Vec::new();
    fs::File::open(&path)
        .await
        .ok()?
        .take(MAX_DOCKERFILE_BYTES + 1)
        .read_to_end(&mut contents)
        .await
        .ok()?;
    if contents.len() as u64 > MAX_DOCKERFILE_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&contents).into_owned())
}

async fn detected_manifests(source: &Path) -> Result<Vec<String>, RuntimeError> {
    const CANDIDATES: &[&str] = &[
        ".env.example",
        "Cargo.toml",
        "Dockerfile",
        "README.md",
        "bun.lock",
        "bun.lockb",
        "composer.json",
        "composer.lock",
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
        "go.mod",
        "package-lock.json",
        "package.json",
        "pnpm-lock.yaml",
        "poetry.lock",
        "pyproject.toml",
        "requirements.txt",
        "uv.lock",
        "yarn.lock",
    ];

    let mut manifests = Vec::new();
    for candidate in CANDIDATES {
        // Only the presence of a name is read here, so a symlink cannot leak.
        if fs::symlink_metadata(source.join(candidate)).await.is_ok() {
            manifests.push((*candidate).to_owned());
        }
    }
    Ok(manifests)
}

fn detect_package_manager(manifests: &[String]) -> Option<&'static str> {
    for (manifest, package_manager) in [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
        ("package-lock.json", "npm"),
        ("composer.lock", "composer"),
        ("uv.lock", "uv"),
        ("poetry.lock", "poetry"),
        ("Cargo.toml", "cargo"),
        ("go.mod", "go"),
    ] {
        if manifests.iter().any(|item| item == manifest) {
            return Some(package_manager);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;

    use super::{RegistryLookup, exposed_ports};
    use crate::{
        CommandSpec, ProcessOutput, ProcessOutputSink, ProcessRunner, RuntimeError,
        TokioProcessRunner,
    };

    /// A registry where every image is single-platform and exposes port 80.
    #[derive(Default)]
    struct CountingRegistry {
        references: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ProcessRunner for CountingRegistry {
        async fn run(
            &self,
            spec: &CommandSpec,
            _sink: &dyn ProcessOutputSink,
        ) -> Result<ProcessOutput, RuntimeError> {
            let reference = spec
                .args
                .last()
                .map(|argument| argument.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.references.lock().expect("lock").push(reference);
            Ok(ProcessOutput {
                success: true,
                code: Some(0),
                stdout: r#"{"config": {"ExposedPorts": {"80/tcp": {}}}}"#.to_owned(),
                stderr: String::new(),
            })
        }
    }

    async fn ports_with_fake(dockerfile: &str) -> (Vec<u16>, Vec<String>) {
        let registry = Arc::new(CountingRegistry::default());
        let lookup = RegistryLookup::new(Arc::clone(&registry) as Arc<dyn ProcessRunner>);
        let ports = exposed_ports(dockerfile, &lookup).await;
        let references = registry.references.lock().expect("lock").clone();
        (ports, references)
    }

    #[tokio::test]
    async fn a_final_stage_without_expose_takes_the_ports_of_its_base_image() {
        let (ports, references) = ports_with_fake(
            "FROM node:22 AS build\nEXPOSE 3000\nRUN npm run build\nFROM nginx:alpine AS web\nFROM web\n",
        )
        .await;
        assert_eq!(ports, [80]);
        assert_eq!(
            references,
            [
                "docker.io/library/nginx:alpine",
                "docker.io/library/nginx:alpine"
            ]
        );
    }

    #[tokio::test]
    async fn the_registry_is_not_asked_when_it_cannot_matter_or_is_not_allowed() {
        for dockerfile in [
            "FROM nginx:alpine\nEXPOSE 8080\n",
            "FROM nginx:alpine\nEXPOSE $PORT\n",
            "FROM scratch\n",
            "FROM nginx:${VERSION}\n",
            "ARG IMAGE\nFROM $IMAGE\n",
            "FROM localhost:5000/x\n",
            "FROM 10.0.0.1/x\n",
            "FROM docker.io.evil.com/x\n",
            "FROM registry.example.com/team/app:1\n",
            "",
        ] {
            let (_, references) = ports_with_fake(dockerfile).await;
            assert!(references.is_empty(), "{dockerfile:?}: {references:?}");
        }
        let (ports, _) = ports_with_fake("FROM nginx:alpine\nEXPOSE 8080\n").await;
        assert_eq!(ports, [8080]);
    }

    async fn real_ports(dockerfile: &str) -> (Vec<u16>, Duration) {
        let lookup =
            RegistryLookup::new(Arc::new(TokioProcessRunner::default()) as Arc<dyn ProcessRunner>);
        let started = std::time::Instant::now();
        let ports = exposed_ports(dockerfile, &lookup).await;
        let elapsed = started.elapsed();
        println!(
            "{:?} -> {ports:?} in {elapsed:?}",
            dockerfile.lines().last()
        );
        (ports, elapsed)
    }

    #[tokio::test]
    #[ignore = "needs Docker Buildx and access to Docker Hub; costs four anonymous manifest requests"]
    async fn real_registry_lookup_of_web_server_base_images() {
        let (nginx, _) = real_ports(
            "FROM node:22-alpine AS build\nEXPOSE 3000\nRUN echo ok > /index.html\nFROM nginx:alpine\nCOPY --from=build /index.html /usr/share/nginx/html/\n",
        )
        .await;
        assert_eq!(nginx, [80]);
        let (httpd, _) =
            real_ports("FROM httpd:2.4-alpine\nCOPY . /usr/local/apache2/htdocs/\n").await;
        assert_eq!(httpd, [80]);
    }

    #[tokio::test]
    #[ignore = "needs Docker Buildx and access to Docker Hub; costs two anonymous manifest requests"]
    async fn real_registry_lookup_of_an_image_without_ports() {
        let (node, _) =
            real_ports("FROM node:22-alpine\nCOPY . /app\nCMD [\"node\", \"/app\"]\n").await;
        assert!(node.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs Docker Buildx and access to public.ecr.aws and quay.io"]
    async fn real_registry_lookup_outside_docker_hub() {
        let (nginx, _) = real_ports("FROM public.ecr.aws/nginx/nginx:stable-alpine\n").await;
        assert_eq!(nginx, [80]);
        let (exporter, _) = real_ports("FROM quay.io/prometheus/node-exporter:v1.8.2\n").await;
        assert_eq!(exporter, [9100]);
    }

    /// The real runner is wrapped so any process it would start is counted.
    #[tokio::test]
    #[ignore = "uses the real process runner"]
    async fn real_lookup_of_a_local_registry_starts_no_process() {
        struct Recording(TokioProcessRunner, Mutex<usize>);
        #[async_trait]
        impl ProcessRunner for Recording {
            async fn run(
                &self,
                spec: &CommandSpec,
                sink: &dyn ProcessOutputSink,
            ) -> Result<ProcessOutput, RuntimeError> {
                *self.1.lock().expect("lock") += 1;
                self.0.run(spec, sink).await
            }
        }
        let runner = Arc::new(Recording(TokioProcessRunner::default(), Mutex::new(0)));
        let lookup = RegistryLookup::new(Arc::clone(&runner) as Arc<dyn ProcessRunner>);
        let started = std::time::Instant::now();
        let ports = exposed_ports("FROM localhost:5000/x\n", &lookup).await;
        println!("localhost:5000/x -> {ports:?} in {:?}", started.elapsed());
        assert!(ports.is_empty());
        assert_eq!(*runner.1.lock().expect("lock"), 0);
    }
}
