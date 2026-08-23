use std::path::Path;

use uuid::Uuid;

use crate::CommandSpec;

pub fn info_command(source: &Path, info: &Path) -> CommandSpec {
    CommandSpec::new("railpack")
        .arg("info")
        .arg("--format")
        .arg("json")
        .arg("--out")
        .arg(info.as_os_str())
        .arg(source.as_os_str())
}

pub fn prepare_command(source: &Path, plan: &Path, info: &Path) -> CommandSpec {
    CommandSpec::new("railpack")
        .arg("prepare")
        .arg(source.as_os_str())
        .arg("--plan-out")
        .arg(plan.as_os_str())
        .arg("--info-out")
        .arg(info.as_os_str())
}

pub fn build_command(
    source: &Path,
    plan: &Path,
    image: &str,
    frontend: &str,
    project_id: Uuid,
    deployment_id: Uuid,
) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("buildx")
        .arg("build")
        .arg("--load")
        .arg("--progress")
        .arg("plain")
        .arg("--build-arg")
        .arg(format!("BUILDKIT_SYNTAX={frontend}"))
        .arg("--file")
        .arg(plan.as_os_str())
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
