//! Reads the ports a public registry image exposes, from its config in the registry.
//!
//! A Dockerfile that ends in `FROM nginx:alpine` without an `EXPOSE` of its own still
//! builds an image that exposes port 80, because the nginx image does. The reference
//! comes from an untrusted repository, so the lookup is narrow: only images on a fixed
//! list of public registries, no credentials of the node or a tenant, nothing pulled
//! or built, one reference per inspection, and a hard deadline. Any failure just means
//! no port was found.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use oci_spec::distribution::Reference;
use serde_json::Value;
use tokio::{fs, time::Instant};
use uuid::Uuid;

use super::dockerfile::tcp_ports;
use crate::{CommandSpec, NullOutputSink, ProcessRunner};

/// Registries anyone can read without an account. Anything else, including IP
/// addresses, `localhost`, and hosts with a port, is never contacted.
pub const PUBLIC_REGISTRIES: [&str; 6] = [
    "docker.io",
    "ghcr.io",
    "quay.io",
    "public.ecr.aws",
    "mcr.microsoft.com",
    "registry.gitlab.com",
];

/// For the whole lookup, both registry requests included.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Same bound as for `EXPOSE`: more entries cannot change what the API does.
const MAX_PORTS: usize = 32;
const MAX_REFERENCE_BYTES: usize = 512;

/// An image reference on one of [`PUBLIC_REGISTRIES`], normalised the way Docker
/// reads it, so the registry checked here is the registry Docker contacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicImage {
    registry: &'static str,
    repository: String,
    tag: Option<String>,
    digest: Option<String>,
}

impl PublicImage {
    /// `None` for anything not plainly on an allowed registry.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > MAX_REFERENCE_BYTES || raw.contains('$') {
            return None;
        }
        // `ghcr.io@sha256:...` or `quay.io:latest` names a Docker Hub image called
        // `library/ghcr.io` to Docker. Whoever wrote it meant the host, so it is
        // skipped rather than sent to either.
        let name = raw.split_once('@').map_or(raw, |(name, _)| name);
        let name = match name.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => name,
            _ => name,
        };
        if !name.contains('/') && (name.contains('.') || name == "localhost") {
            return None;
        }

        let reference = Reference::try_from(raw).ok()?;
        let registry = PUBLIC_REGISTRIES
            .into_iter()
            .find(|registry| *registry == reference.registry())?;
        // Docker reads an uppercase first component as a host, the parser does not.
        if reference
            .repository()
            .bytes()
            .any(|byte| !matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'/'))
        {
            return None;
        }
        Some(Self {
            registry,
            repository: reference.repository().to_owned(),
            tag: reference.tag().map(str::to_owned),
            digest: reference.digest().map(str::to_owned),
        })
    }

    fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// The fully qualified reference, so Docker has nothing left to guess.
    fn reference(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(digest), _) => format!("{}@{digest}", self.name()),
            (None, Some(tag)) => format!("{}:{tag}", self.name()),
            (None, None) => format!("{}:latest", self.name()),
        }
    }
}

/// Reads image configs with `docker buildx imagetools inspect`, which talks to the
/// registry directly and never to the Docker daemon, so nothing is pulled.
pub struct RegistryLookup {
    runner: Arc<dyn ProcessRunner>,
    timeout: Duration,
    architecture: &'static str,
    scratch_root: PathBuf,
}

impl RegistryLookup {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            runner,
            timeout: LOOKUP_TIMEOUT,
            architecture: node_architecture(),
            scratch_root: std::env::temp_dir(),
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_scratch_root(mut self, root: &Path) -> Self {
        root.clone_into(&mut self.scratch_root);
        self
    }

    #[cfg(test)]
    fn with_architecture(mut self, architecture: &'static str) -> Self {
        self.architecture = architecture;
        self
    }

    /// TCP ports in the image's `Config.ExposedPorts`, sorted. Empty on any failure.
    pub async fn exposed_ports(&self, image: &PublicImage) -> Vec<u16> {
        let Some(config) = EmptyDockerConfig::create(&self.scratch_root).await else {
            tracing::warn!("base image lookup skipped: no scratch directory for Docker");
            return Vec::new();
        };
        let found = tokio::time::timeout(self.timeout, self.lookup(image, config.path())).await;
        config.remove().await;
        match found {
            Ok(Some(ports)) => ports,
            Ok(None) => {
                tracing::info!(image = %image.reference(), "base image lookup found nothing");
                Vec::new()
            }
            Err(_) => {
                tracing::info!(image = %image.reference(), "base image lookup timed out");
                Vec::new()
            }
        }
    }

    async fn lookup(&self, image: &PublicImage, config: &Path) -> Option<Vec<u16>> {
        let deadline = Instant::now() + self.timeout;
        // Asking for every platform's config at once costs a manifest request per
        // platform, and Docker Hub counts each against the node's anonymous limit.
        // The index plus one platform costs two.
        let reference = image.reference();
        let manifest = self
            .inspect(&reference, &["--raw"], config, deadline)
            .await?;
        let target = if manifest.get("manifests").is_some() {
            let digest = platform_digest(&manifest, self.architecture)?;
            format!("{}@{digest}", image.name())
        } else {
            reference
        };
        let image = self
            .inspect(&target, &["--format", "{{json .Image}}"], config, deadline)
            .await?;
        Some(exposed_ports(&image, self.architecture))
    }

    async fn inspect(
        &self,
        reference: &str,
        options: &[&str],
        config: &Path,
        deadline: Instant,
    ) -> Option<Value> {
        let mut command = CommandSpec::new("docker")
            .without_tool_environment()
            .env("DOCKER_CONFIG", config.to_string_lossy())
            .env("BUILDX_CONFIG", config.join("buildx").to_string_lossy())
            .arg("buildx")
            .arg("imagetools")
            .arg("inspect");
        for option in options {
            command = command.arg(*option);
        }
        let command = command
            .arg(reference)
            .timeout(deadline.saturating_duration_since(Instant::now()));
        let output = self.runner.run(&command, &NullOutputSink).await.ok()?;
        if !output.success {
            return None;
        }
        serde_json::from_str(&output.stdout).ok()
    }
}

/// The digest of the manifest for this node's Linux architecture, or for
/// `linux/amd64` when the index has none. The index comes from the registry, so the
/// digest is checked before it goes into an argument.
fn platform_digest(index: &Value, architecture: &str) -> Option<String> {
    let manifests = index.get("manifests")?.as_array()?;
    let digest_for = |architecture: &str| {
        manifests.iter().find_map(|manifest| {
            let platform = manifest.get("platform")?;
            (platform.get("os")?.as_str()? == "linux"
                && platform.get("architecture")?.as_str()? == architecture)
                .then(|| manifest.get("digest")?.as_str())
                .flatten()
        })
    };
    let digest = digest_for(architecture).or_else(|| digest_for("amd64"))?;
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    .then(|| digest.to_owned())
}

/// `.Image` is one config for a single-platform manifest, and a map by platform for
/// an index. Only TCP ports count, as for `EXPOSE`.
fn exposed_ports(image: &Value, architecture: &str) -> Vec<u16> {
    let config = if image.get("config").is_some() {
        Some(image)
    } else {
        image
            .get(format!("linux/{architecture}"))
            .or_else(|| image.get("linux/amd64"))
    };
    let Some(ports) = config
        .and_then(|config| config.get("config"))
        .and_then(|config| config.get("ExposedPorts"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    ports
        .keys()
        .flat_map(|spec| tcp_ports(spec))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_PORTS)
        .collect()
}

/// The architecture name images use for this node.
fn node_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "arm" => "arm",
        "x86" => "386",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        _ => "amd64",
    }
}

/// A Docker config directory with no credentials, credential helpers, contexts, or
/// builders, created for one lookup and removed after it. Buildx writes its state
/// there, so it cannot be shared with the operator's.
struct EmptyDockerConfig {
    path: PathBuf,
}

impl EmptyDockerConfig {
    async fn create(root: &Path) -> Option<Self> {
        let path = root.join(format!("sakala-agent-registry-{}", Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&path).await.ok()?;
        let config = Self { path };
        // Buildx may be installed only in the operator's own plugin directory, which
        // Docker looks for under the config directory. The plugin is the same binary
        // every build runs; nothing else from that directory is used.
        if let Some(plugins) = operator_plugin_directory()
            && fs::metadata(&plugins)
                .await
                .is_ok_and(|metadata| metadata.is_dir())
        {
            let contents = serde_json::json!({ "cliPluginsExtraDirs": [plugins] });
            if fs::write(config.path.join("config.json"), contents.to_string())
                .await
                .is_err()
            {
                config.remove().await;
                return None;
            }
        }
        Some(config)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn remove(self) {
        let _ = fs::remove_dir_all(&self.path).await;
    }
}

fn operator_plugin_directory() -> Option<PathBuf> {
    let config = std::env::var_os("DOCKER_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".docker")))?;
    Some(config.join("cli-plugins"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::{ProcessOutput, ProcessOutputSink, RuntimeError};

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ARM_DIGEST: &str =
        "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn parsed(raw: &str) -> Option<String> {
        PublicImage::parse(raw).map(|image| image.reference())
    }

    #[test]
    fn docker_hub_images_are_qualified_as_docker_reads_them() {
        assert_eq!(
            parsed("nginx:alpine").as_deref(),
            Some("docker.io/library/nginx:alpine")
        );
        assert_eq!(
            parsed("nginx").as_deref(),
            Some("docker.io/library/nginx:latest")
        );
        assert_eq!(
            parsed("bitnami/nginx:1.27").as_deref(),
            Some("docker.io/bitnami/nginx:1.27")
        );
        assert_eq!(
            parsed("docker.io/httpd:2.4").as_deref(),
            Some("docker.io/library/httpd:2.4")
        );
        assert_eq!(
            parsed("index.docker.io/library/httpd:2.4").as_deref(),
            Some("docker.io/library/httpd:2.4")
        );
    }

    #[test]
    fn every_public_registry_on_the_list_is_accepted() {
        for raw in [
            "ghcr.io/railwayapp/railpack-frontend:v0.23.0",
            "quay.io/prometheus/node-exporter:v1.8.2",
            "public.ecr.aws/nginx/nginx:stable",
            "mcr.microsoft.com/dotnet/aspnet:8.0",
            "registry.gitlab.com/group/project/image:main",
        ] {
            assert_eq!(parsed(raw).as_deref(), Some(raw), "{raw}");
        }
    }

    #[test]
    fn a_digest_wins_over_a_tag() {
        assert_eq!(
            parsed(&format!("nginx:alpine@{DIGEST}")),
            Some(format!("docker.io/library/nginx@{DIGEST}"))
        );
        assert_eq!(
            parsed(&format!("ghcr.io/owner/app@{DIGEST}")),
            Some(format!("ghcr.io/owner/app@{DIGEST}"))
        );
    }

    #[test]
    fn other_hosts_are_never_contacted() {
        for raw in [
            "docker.io.evil.com/x",
            "evil.com/docker.io/x",
            "ghcr.io.evil.com/owner/app",
            "localhost:5000/x",
            "localhost/x",
            "10.0.0.1/x",
            "10.0.0.1:5000/x",
            "[::1]:5000/x",
            "ghcr.io:443/owner/app",
            "docker.io:443/library/nginx",
            "GHCR.IO/owner/app",
            "Ghcr.io/owner/app",
            "quay.io./x",
            "registry.example.com/team/app:1",
            "registry-1.docker.io/library/nginx",
        ] {
            assert_eq!(parsed(raw), None, "{raw}");
        }
    }

    #[test]
    fn a_registry_name_without_a_path_is_ambiguous_and_skipped() {
        assert_eq!(parsed(&format!("ghcr.io@{DIGEST}")), None);
        assert_eq!(parsed("ghcr.io"), None);
        assert_eq!(parsed("quay.io:latest"), None);
        assert_eq!(parsed("localhost"), None);
    }

    #[test]
    fn malformed_and_unresolved_references_are_skipped() {
        for raw in [
            "",
            "nginx:${VERSION}",
            "$BASE",
            "Nginx:alpine",
            "Owner/app",
            "nginx:alpine --flag",
            "-nginx",
            "user:pass@ghcr.io/owner/app",
            "nginx@sha256:short",
            "nginx@md5:0123456789abcdef0123456789abcdef",
            "ghcr.io/owner/app:tag/with/slash",
        ] {
            assert_eq!(parsed(raw), None, "{raw}");
        }
        assert_eq!(parsed(&format!("ghcr.io/{}", "a".repeat(600))), None);
    }

    #[test]
    fn the_node_architecture_is_picked_from_an_index() {
        let index = json!({
            "manifests": [
                {"digest": "sha256:unknown", "platform": {"os": "unknown", "architecture": "unknown"}},
                {"digest": ARM_DIGEST, "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}},
                {"digest": DIGEST, "platform": {"os": "linux", "architecture": "amd64"}},
            ]
        });
        assert_eq!(platform_digest(&index, "amd64").as_deref(), Some(DIGEST));
        assert_eq!(
            platform_digest(&index, "arm64").as_deref(),
            Some(ARM_DIGEST)
        );
        // Without the node's own, amd64 is the fallback.
        assert_eq!(platform_digest(&index, "s390x").as_deref(), Some(DIGEST));
        assert_eq!(platform_digest(&json!({"manifests": []}), "amd64"), None);
    }

    #[test]
    fn a_digest_from_the_registry_is_checked_before_use() {
        for digest in [
            "sha256:abc",
            "sha256:../../x",
            "--help",
            &DIGEST.to_uppercase(),
        ] {
            let index = json!({
                "manifests": [{"digest": digest, "platform": {"os": "linux", "architecture": "amd64"}}]
            });
            assert_eq!(platform_digest(&index, "amd64"), None, "{digest}");
        }
    }

    #[test]
    fn only_tcp_ports_of_the_config_count() {
        let image =
            json!({"config": {"ExposedPorts": {"80/tcp": {}, "443/tcp": {}, "53/udp": {}}}});
        assert_eq!(exposed_ports(&image, "amd64"), [80, 443]);
        assert!(exposed_ports(&json!({"config": {}}), "amd64").is_empty());
        assert!(exposed_ports(&json!({"config": {"ExposedPorts": null}}), "amd64").is_empty());
        let by_platform = json!({
            "linux/386": {"config": {"ExposedPorts": {"1/tcp": {}}}},
            "linux/amd64": {"config": {"ExposedPorts": {"80/tcp": {}}}},
        });
        assert_eq!(exposed_ports(&by_platform, "amd64"), [80]);
    }

    /// Answers `--raw` with an index and `--format` with a config, like a registry
    /// would, and records every command.
    struct FakeRegistry {
        commands: Mutex<Vec<CommandSpec>>,
        config_existed: Mutex<Vec<bool>>,
        index: Option<Value>,
        image: Value,
        delay: Option<Duration>,
        fail: bool,
        spawn_error: bool,
    }

    impl FakeRegistry {
        fn new() -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                config_existed: Mutex::new(Vec::new()),
                index: Some(json!({
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "manifests": [
                        {"digest": DIGEST, "platform": {"os": "linux", "architecture": "amd64"}},
                    ]
                })),
                image: json!({"config": {"ExposedPorts": {"80/tcp": {}}}}),
                delay: None,
                fail: false,
                spawn_error: false,
            }
        }

        fn commands(&self) -> Vec<CommandSpec> {
            self.commands.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl ProcessRunner for FakeRegistry {
        async fn run(
            &self,
            spec: &CommandSpec,
            _sink: &dyn ProcessOutputSink,
        ) -> Result<ProcessOutput, RuntimeError> {
            self.commands.lock().expect("lock").push(spec.clone());
            let config = spec.environment.get("DOCKER_CONFIG").map(PathBuf::from);
            self.config_existed
                .lock()
                .expect("lock")
                .push(config.is_some_and(|config| config.is_dir()));
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            if self.spawn_error {
                return Err(RuntimeError::Dependency(
                    "could not start docker".to_owned(),
                ));
            }
            let raw = spec.args.iter().any(|argument| argument == "--raw");
            let stdout = match (raw, &self.index) {
                (true, Some(index)) => serde_json::to_string_pretty(index).expect("json"),
                (true, None) => serde_json::to_string_pretty(&json!({
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": {"digest": DIGEST}
                }))
                .expect("json"),
                (false, _) => serde_json::to_string_pretty(&self.image).expect("json"),
            };
            Ok(ProcessOutput {
                success: !self.fail,
                code: Some(i32::from(self.fail)),
                stdout: format!("{stdout}\n"),
                stderr: String::new(),
            })
        }
    }

    fn lookup(registry: &Arc<FakeRegistry>, scratch: &Path) -> RegistryLookup {
        RegistryLookup::new(Arc::clone(registry) as Arc<dyn ProcessRunner>)
            .with_scratch_root(scratch)
            .with_architecture("amd64")
    }

    fn image(raw: &str) -> PublicImage {
        PublicImage::parse(raw).expect("allowed reference")
    }

    fn arguments(spec: &CommandSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn an_index_takes_one_request_for_it_and_one_for_the_platform() {
        let scratch = tempfile::TempDir::new().expect("temp");
        let registry = Arc::new(FakeRegistry::new());

        let ports = lookup(&registry, scratch.path())
            .exposed_ports(&image("nginx:alpine"))
            .await;

        assert_eq!(ports, [80]);
        let commands = registry.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            arguments(&commands[0]),
            [
                "buildx",
                "imagetools",
                "inspect",
                "--raw",
                "docker.io/library/nginx:alpine"
            ]
        );
        assert_eq!(
            arguments(&commands[1]),
            [
                "buildx",
                "imagetools",
                "inspect",
                "--format",
                "{{json .Image}}",
                &format!("docker.io/library/nginx@{DIGEST}"),
            ]
        );
    }

    #[tokio::test]
    async fn a_single_platform_manifest_is_read_by_its_own_reference() {
        let scratch = tempfile::TempDir::new().expect("temp");
        let mut fake = FakeRegistry::new();
        fake.index = None;
        fake.image = json!({"config": {"ExposedPorts": {"9100/tcp": {}}}});
        let registry = Arc::new(fake);

        let ports = lookup(&registry, scratch.path())
            .exposed_ports(&image("quay.io/prometheus/node-exporter:v1.8.2"))
            .await;

        assert_eq!(ports, [9100]);
        let commands = registry.commands();
        assert_eq!(
            arguments(&commands[1]).last().map(String::as_str),
            Some("quay.io/prometheus/node-exporter:v1.8.2")
        );
    }

    #[tokio::test]
    async fn the_lookup_uses_no_credentials_and_cleans_up() {
        let scratch = tempfile::TempDir::new().expect("temp");
        let registry = Arc::new(FakeRegistry::new());

        lookup(&registry, scratch.path())
            .exposed_ports(&image("httpd:2.4-alpine"))
            .await;

        for command in registry.commands() {
            assert_eq!(command.program, "docker");
            assert!(!command.inherits_tool_environment());
            assert_eq!(command.secret_environment_names().count(), 0);
            let config = Path::new(&command.environment["DOCKER_CONFIG"]).to_owned();
            assert!(config.starts_with(scratch.path()), "{config:?}");
            assert_eq!(
                Path::new(&command.environment["BUILDX_CONFIG"]),
                config.join("buildx")
            );
            assert!(
                command
                    .timeout
                    .is_some_and(|timeout| timeout <= LOOKUP_TIMEOUT)
            );
        }
        assert_eq!(*registry.config_existed.lock().expect("lock"), [true, true]);
        assert_eq!(
            std::fs::read_dir(scratch.path()).expect("scratch").count(),
            0,
            "the config directory is removed"
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_finds_no_port() {
        let scratch = tempfile::TempDir::new().expect("temp");
        let mut failing = FakeRegistry::new();
        failing.fail = true;
        let mut missing_docker = FakeRegistry::new();
        missing_docker.spawn_error = true;
        let mut garbage = FakeRegistry::new();
        garbage.index = Some(json!({"manifests": "not a list"}));
        let mut no_ports = FakeRegistry::new();
        no_ports.image = json!({"config": {"Env": ["PATH=/bin"]}});

        for (name, fake) in [
            ("exit 1", failing),
            ("no docker", missing_docker),
            ("bad index", garbage),
            ("no ports", no_ports),
        ] {
            let registry = Arc::new(fake);
            let ports = lookup(&registry, scratch.path())
                .exposed_ports(&image("node:22-alpine"))
                .await;
            assert!(ports.is_empty(), "{name}");
        }
        assert_eq!(
            std::fs::read_dir(scratch.path()).expect("scratch").count(),
            0
        );
    }

    #[tokio::test]
    async fn a_slow_registry_hits_the_deadline() {
        let scratch = tempfile::TempDir::new().expect("temp");
        let mut slow = FakeRegistry::new();
        slow.delay = Some(Duration::from_secs(60));
        let registry = Arc::new(slow);
        let started = std::time::Instant::now();

        let ports = lookup(&registry, scratch.path())
            .with_timeout(Duration::from_millis(100))
            .exposed_ports(&image("nginx:alpine"))
            .await;

        assert!(ports.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(registry.commands().len(), 1);
        assert_eq!(
            std::fs::read_dir(scratch.path()).expect("scratch").count(),
            0
        );
    }

    #[tokio::test]
    async fn no_scratch_directory_means_no_lookup() {
        let registry = Arc::new(FakeRegistry::new());
        let ports = lookup(&registry, Path::new("/nonexistent/sakala-scratch"))
            .exposed_ports(&image("nginx:alpine"))
            .await;
        assert!(ports.is_empty());
        assert!(registry.commands().is_empty());
    }
}
