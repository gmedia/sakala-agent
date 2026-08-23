use std::path::Path;

use uuid::Uuid;

use crate::CommandSpec;

pub fn build_command(
    source: &Path,
    dockerfile: &Path,
    image: &str,
    project_id: Uuid,
    deployment_id: Uuid,
) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("buildx")
        .arg("build")
        .arg("--load")
        .arg("--progress")
        .arg("plain")
        .arg("--file")
        .arg(dockerfile.as_os_str())
        .arg("--tag")
        .arg(image)
        .arg("--label")
        .arg("dev.sakala.managed=true")
        .arg("--label")
        .arg(format!("dev.sakala.project-id={project_id}"))
        .arg("--label")
        .arg(format!("dev.sakala.deployment-id={deployment_id}"))
        .arg(source.as_os_str())
}
