//! Dependency names from JSON manifests (`package.json`, `composer.json`) in the
//! inspected directory, read with the bounded rules of [`repository_file`]. Only the
//! presence of a key is looked at: no version is parsed and nothing is executed.

use std::path::Path;

use serde_json::Value;

use crate::repository_file::{self, MAX_MANIFEST_BYTES};

pub(super) const PACKAGE_JSON: &str = "package.json";
/// The `package.json` sections whose keys count as dependencies.
pub(super) const PACKAGE_JSON_SECTIONS: [&str; 2] = ["dependencies", "devDependencies"];

/// The JSON file `relative` to the canonical `root`, `None` when it is missing, leaves
/// `root`, is not a regular file, is larger than [`MAX_MANIFEST_BYTES`], or is not
/// valid UTF-8 JSON.
pub(super) async fn read(root: &Path, relative: &str) -> Option<Value> {
    let contents =
        repository_file::read_text(root, &root.join(relative), MAX_MANIFEST_BYTES).await?;
    serde_json::from_str(&contents).ok()
}

/// Whether one of `sections` of `manifest` is an object with the key `package`.
pub(super) fn has_package(manifest: &Value, sections: &[&str], package: &str) -> bool {
    sections.iter().any(|section| {
        manifest
            .get(section)
            .and_then(Value::as_object)
            .is_some_and(|dependencies| dependencies.contains_key(package))
    })
}
