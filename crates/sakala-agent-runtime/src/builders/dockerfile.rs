use std::path::Path;

use crate::CommandSpec;

pub fn build_command(source: &Path, dockerfile: &Path, image: &str) -> CommandSpec {
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
        .arg(source.as_os_str())
}
