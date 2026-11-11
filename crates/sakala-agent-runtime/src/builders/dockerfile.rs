use std::path::{Path, PathBuf};

use tokio::fs;
use uuid::Uuid;

use crate::{CommandSpec, RuntimeError, builders::with_tenant_isolation};

const MAX_DOCKERFILE_BYTES: u64 = 1024 * 1024;
const FRONTEND_LOCK: &str =
    "# Sakala fixed the build frontend; parser directives below this line are ignored.\n";

/// Writes the copy of a Dockerfile that gets built, in which `# syntax=` has no
/// effect. That line chooses the program that turns a Dockerfile into a build, and
/// such a program can ignore `--no-cache`, `--pull`, and the cache namespace, so a
/// repository must not choose it. BuildKit only looks for directives until the
/// first ordinary comment, which is why a comment on top is enough.
pub async fn write_locked_dockerfile(
    dockerfile: &Path,
    workspace: &Path,
) -> Result<PathBuf, RuntimeError> {
    if fs::metadata(dockerfile).await?.len() > MAX_DOCKERFILE_BYTES {
        return Err(RuntimeError::Build(
            "the Dockerfile is larger than the 1 MiB limit".to_owned(),
        ));
    }
    let content = fs::read(dockerfile).await?;
    let directory = workspace.join(format!("dockerfile-{}", Uuid::new_v4()));
    fs::create_dir(&directory).await?;
    let locked = directory.join("Dockerfile");
    fs::write(&locked, lock_frontend(&content)).await?;
    Ok(locked)
}

fn lock_frontend(content: &[u8]) -> Vec<u8> {
    const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";
    let content = content.strip_prefix(BYTE_ORDER_MARK).unwrap_or(content);
    let mut locked = Vec::with_capacity(content.len() + FRONTEND_LOCK.len() + 16);
    // `escape` changes how the file parses, and it only works at the very top.
    if let Some(escape) = escape_directive(content) {
        locked.extend_from_slice(format!("# escape={escape}\n").as_bytes());
    }
    locked.extend_from_slice(FRONTEND_LOCK.as_bytes());
    locked.extend_from_slice(content);
    locked
}

fn escape_directive(content: &[u8]) -> Option<char> {
    for line in String::from_utf8_lossy(content).lines() {
        let (key, value) = line.trim().strip_prefix('#')?.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("escape") {
            return match value.trim() {
                "`" => Some('`'),
                "\\" => Some('\\'),
                _ => None,
            };
        }
    }
    None
}

pub fn build_command(
    source: &Path,
    dockerfile: &Path,
    image: &str,
    project_id: Uuid,
    deployment_id: Uuid,
) -> CommandSpec {
    let command = CommandSpec::new("docker")
        .arg("buildx")
        .arg("build")
        .arg("--load")
        .arg("--progress")
        .arg("plain");
    with_tenant_isolation(command)
        .arg("--build-arg")
        .arg(format!("BUILDKIT_CACHE_MOUNT_NS={project_id}"))
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

#[cfg(test)]
mod tests {
    use super::{FRONTEND_LOCK, lock_frontend};

    fn locked(content: &str) -> String {
        String::from_utf8(lock_frontend(content.as_bytes())).expect("text")
    }

    #[test]
    fn every_kind_of_syntax_directive_ends_up_below_a_plain_comment() {
        for content in [
            "# syntax=evil/frontend\nFROM scratch\n",
            "#syntax=evil/frontend\nFROM scratch\n",
            "// syntax=evil/frontend\nFROM scratch\n",
            "\u{feff}# syntax=evil/frontend\nFROM scratch\n",
            "{\"syntax\":\"evil/frontend\"}",
        ] {
            let locked = locked(content);
            assert!(locked.starts_with(FRONTEND_LOCK), "{locked}");
            assert!(!locked.contains('\u{feff}'), "{locked}");
        }
    }

    #[test]
    fn the_escape_directive_keeps_working_and_nothing_else_moves() {
        let locked = locked("# syntax=evil/frontend\n# escape=`\nFROM scratch\n");
        let mut lines = locked.lines();
        assert_eq!(lines.next(), Some("# escape=`"));
        assert_eq!(
            lines.next().map(|line| format!("{line}\n")).as_deref(),
            Some(FRONTEND_LOCK)
        );
        assert_eq!(lines.next(), Some("# syntax=evil/frontend"));
    }

    #[test]
    fn a_value_that_is_not_an_escape_character_is_not_carried_over() {
        let locked = locked("# escape=x\n# syntax=evil\nFROM scratch\n");
        assert!(locked.starts_with(FRONTEND_LOCK), "{locked}");
    }

    #[test]
    fn an_escape_directive_below_an_instruction_is_not_one() {
        let locked = locked("FROM scratch\n# escape=`\n");
        assert!(locked.starts_with(FRONTEND_LOCK), "{locked}");
    }
}
