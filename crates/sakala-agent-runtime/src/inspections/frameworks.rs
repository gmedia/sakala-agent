//! Names the frameworks that can render pages at build time from the dependencies of
//! the inspected directory's `package.json`, so the API knows a variable change there
//! needs a new build. Railpack reports several of them (Gatsby, Astro in server mode,
//! Eleventy, Docusaurus, VitePress) as plain `node`.
//!
//! Only the keys of `dependencies` and `devDependencies` are looked at, through the
//! bounded reads of [`json_manifest`]. A missing, unreadable, oversized, or invalid
//! `package.json` gives `[]`, never a failure.

use std::{collections::BTreeSet, path::Path};

use sakala_agent_protocol::JsFramework;
use serde_json::Value;
use tokio::fs;

use super::json_manifest::{self, PACKAGE_JSON, PACKAGE_JSON_SECTIONS, has_package};

/// Exact package names and the framework each one means. Libraries that are also
/// used without the framework's build (`react-router`, `@remix-run/react`) are not
/// listed: only the framework's build tooling counts.
const FRAMEWORK_PACKAGES: [(&str, JsFramework); 14] = [
    ("next", JsFramework::Next),
    ("nuxt", JsFramework::Nuxt),
    ("nuxt3", JsFramework::Nuxt),
    ("astro", JsFramework::Astro),
    ("@sveltejs/kit", JsFramework::Sveltekit),
    ("gatsby", JsFramework::Gatsby),
    ("@remix-run/dev", JsFramework::Remix),
    ("@react-router/dev", JsFramework::ReactRouter),
    ("@tanstack/react-start", JsFramework::TanstackStart),
    ("@tanstack/solid-start", JsFramework::TanstackStart),
    ("@tanstack/start", JsFramework::TanstackStart),
    ("@11ty/eleventy", JsFramework::Eleventy),
    ("@docusaurus/core", JsFramework::Docusaurus),
    ("vitepress", JsFramework::Vitepress),
];

/// The frameworks `package.json` in `context` depends on, sorted and unique.
pub(super) async fn detect(context: &Path) -> Vec<JsFramework> {
    let Ok(root) = fs::canonicalize(context).await else {
        return Vec::new();
    };
    match json_manifest::read(&root, PACKAGE_JSON).await {
        Some(manifest) => from_manifest(&manifest),
        None => Vec::new(),
    }
}

fn from_manifest(manifest: &Value) -> Vec<JsFramework> {
    // `JsFramework` orders like its wire names, so the set is already sorted.
    FRAMEWORK_PACKAGES
        .iter()
        .filter(|(package, _)| has_package(manifest, &PACKAGE_JSON_SECTIONS, package))
        .map(|(_, framework)| *framework)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tempfile::TempDir;

    use crate::repository_file::MAX_MANIFEST_BYTES;

    fn dependencies(names: &[&str]) -> Value {
        let section: serde_json::Map<String, Value> = names
            .iter()
            .map(|name| ((*name).to_owned(), json!("^1.0.0")))
            .collect();
        json!({ "dependencies": section })
    }

    async fn detect_files(files: &[(&str, &str)]) -> Vec<JsFramework> {
        let temp = TempDir::new().expect("temp");
        for (path, contents) in files {
            let path = temp.path().join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            std::fs::write(path, contents).expect("file");
        }
        detect(temp.path()).await
    }

    #[test]
    fn every_package_name_maps_to_its_framework() {
        for (package, expected) in [
            ("next", JsFramework::Next),
            ("nuxt", JsFramework::Nuxt),
            ("nuxt3", JsFramework::Nuxt),
            ("astro", JsFramework::Astro),
            ("@sveltejs/kit", JsFramework::Sveltekit),
            ("gatsby", JsFramework::Gatsby),
            ("@remix-run/dev", JsFramework::Remix),
            ("@react-router/dev", JsFramework::ReactRouter),
            ("@tanstack/react-start", JsFramework::TanstackStart),
            ("@tanstack/solid-start", JsFramework::TanstackStart),
            ("@tanstack/start", JsFramework::TanstackStart),
            ("@11ty/eleventy", JsFramework::Eleventy),
            ("@docusaurus/core", JsFramework::Docusaurus),
            ("vitepress", JsFramework::Vitepress),
        ] {
            assert_eq!(
                from_manifest(&dependencies(&[package])),
                [expected],
                "{package}"
            );
        }
    }

    #[test]
    fn every_framework_has_a_package_name() {
        let mapped: BTreeSet<JsFramework> = FRAMEWORK_PACKAGES
            .iter()
            .map(|(_, framework)| *framework)
            .collect();
        assert_eq!(mapped.into_iter().collect::<Vec<_>>(), JsFramework::ALL);
    }

    #[test]
    fn libraries_without_the_framework_build_do_not_count() {
        for names in [
            &["react-router", "react", "react-dom"][..],
            &["@remix-run/react", "@remix-run/node"][..],
            &["vite", "svelte", "@tanstack/react-router", "vue", "express"][..],
            &["Next", "next-auth", "@next/font"][..],
        ] {
            assert!(from_manifest(&dependencies(names)).is_empty(), "{names:?}");
        }
    }

    #[test]
    fn dev_dependencies_count_and_other_sections_do_not() {
        assert_eq!(
            from_manifest(&json!({ "devDependencies": { "@sveltejs/kit": "^2" } })),
            [JsFramework::Sveltekit]
        );
        assert!(
            from_manifest(&json!({
                "peerDependencies": { "next": "^15" },
                "optionalDependencies": { "gatsby": "^5" },
                "scripts": { "build": "astro build" },
                "name": "vitepress"
            }))
            .is_empty()
        );
        // A section that is not an object is ignored, not an error.
        assert!(from_manifest(&json!({ "dependencies": ["next"] })).is_empty());
        assert!(from_manifest(&json!(["next"])).is_empty());
    }

    #[test]
    fn the_result_is_sorted_by_wire_name_and_unique() {
        let manifest = json!({
            "dependencies": {
                "vitepress": "1",
                "@tanstack/start": "1",
                "nuxt3": "1",
                "next": "1",
                "@tanstack/react-start": "1"
            },
            "devDependencies": {
                "nuxt": "1",
                "astro": "1",
                "@tanstack/solid-start": "1"
            }
        });
        let frameworks = from_manifest(&manifest);
        assert_eq!(
            frameworks,
            [
                JsFramework::Astro,
                JsFramework::Next,
                JsFramework::Nuxt,
                JsFramework::TanstackStart,
                JsFramework::Vitepress
            ]
        );
        assert_eq!(
            serde_json::to_value(&frameworks).expect("json"),
            json!(["astro", "next", "nuxt", "tanstack-start", "vitepress"])
        );
    }

    #[tokio::test]
    async fn a_package_json_in_the_inspected_directory_is_read() {
        assert_eq!(
            detect_files(&[(
                "package.json",
                r#"{"dependencies":{"gatsby":"^5"},"devDependencies":{"@11ty/eleventy":"^3"}}"#
            )])
            .await,
            [JsFramework::Eleventy, JsFramework::Gatsby]
        );
    }

    #[tokio::test]
    async fn a_missing_invalid_or_oversized_package_json_gives_no_frameworks() {
        assert!(detect_files(&[]).await.is_empty());
        assert!(detect_files(&[("package.json", "")]).await.is_empty());
        assert!(
            detect_files(&[("package.json", r#"{"dependencies":{"next":"#)])
                .await
                .is_empty()
        );
        // Valid JSON padded past the cap is not read at all.
        let padding = " ".repeat(usize::try_from(MAX_MANIFEST_BYTES).expect("cap fits"));
        let oversized = format!(r#"{{"dependencies":{{"next":"^15"}}}}{padding}"#);
        assert!(
            detect_files(&[("package.json", &oversized)])
                .await
                .is_empty()
        );
        // The same file within the cap is read, so the cap is what made it empty.
        assert_eq!(
            detect_files(&[("package.json", r#"{"dependencies":{"next":"^15"}}"#)]).await,
            [JsFramework::Next]
        );
        let temp = TempDir::new().expect("temp");
        std::fs::create_dir(temp.path().join("package.json")).expect("directory");
        assert!(detect(temp.path()).await.is_empty());
        assert!(detect(&temp.path().join("missing")).await.is_empty());
    }

    #[tokio::test]
    async fn a_symlink_leaving_the_inspected_directory_is_not_followed() {
        let outside = TempDir::new().expect("temp");
        std::fs::write(
            outside.path().join("package.json"),
            r#"{"dependencies":{"next":"^15"}}"#,
        )
        .expect("file");
        let temp = TempDir::new().expect("temp");
        std::os::unix::fs::symlink(
            outside.path().join("package.json"),
            temp.path().join("package.json"),
        )
        .expect("symlink");
        assert!(detect(temp.path()).await.is_empty());

        // A symlink that stays inside is followed.
        let inside = TempDir::new().expect("temp");
        std::fs::write(
            inside.path().join("real.json"),
            r#"{"dependencies":{"next":"^15"}}"#,
        )
        .expect("file");
        std::os::unix::fs::symlink("real.json", inside.path().join("package.json"))
            .expect("symlink");
        assert_eq!(detect(inside.path()).await, [JsFramework::Next]);
    }

    #[tokio::test]
    async fn a_workspace_package_is_read_from_its_own_directory() {
        let temp = TempDir::new().expect("temp");
        for (path, contents) in [
            (
                "package.json",
                r#"{"workspaces":["apps/*"],"devDependencies":{"gatsby":"^5"}}"#,
            ),
            (
                "apps/web/package.json",
                r#"{"name":"web","dependencies":{"astro":"^5"}}"#,
            ),
            ("apps/api/package.json", r#"{"name":"api"}"#),
        ] {
            let path = temp.path().join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            std::fs::write(path, contents).expect("file");
        }
        assert_eq!(
            detect(&temp.path().join("apps/web")).await,
            [JsFramework::Astro]
        );
        assert!(detect(&temp.path().join("apps/api")).await.is_empty());
        std::fs::create_dir_all(temp.path().join("apps/docs")).expect("directory");
        std::os::unix::fs::symlink(
            "../../package.json",
            temp.path().join("apps/docs/package.json"),
        )
        .expect("symlink");
        // The workspace root's file is outside the package directory.
        assert!(detect(&temp.path().join("apps/docs")).await.is_empty());
    }
}
