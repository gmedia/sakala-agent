use std::path::Path;

use uuid::Uuid;

use crate::{
    CommandSpec,
    builders::{with_image_labels, with_tenant_isolation},
};

pub fn build_command(
    source: &Path,
    plan: &Path,
    image: &str,
    frontend: &str,
    project_id: Uuid,
    labels: &[String],
    secrets_fingerprint: Option<&str>,
) -> CommandSpec {
    let command = CommandSpec::new("docker")
        .arg("buildx")
        .arg("build")
        .arg("--load")
        .arg("--progress")
        .arg("plain");
    let mut command = with_tenant_isolation(command)
        .arg("--build-arg")
        .arg(format!("BUILDKIT_SYNTAX={frontend}"))
        // Railpack's frontend prefixes its cache mounts with this.
        .arg("--build-arg")
        .arg(format!("cache-key={project_id}"));
    // BuildKit keeps a cached step when only a secret changed. This is how
    // Railpack's frontend learns that it must not.
    if let Some(fingerprint) = secrets_fingerprint {
        command = command
            .arg("--build-arg")
            .arg(format!("secrets-hash={fingerprint}"));
    }
    let command = command
        .arg("--file")
        .arg(plan.as_os_str())
        .arg("--tag")
        .arg(image);
    with_image_labels(command, labels).arg(source.as_os_str())
}
