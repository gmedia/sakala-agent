mod commands;
mod labels;
mod sandbox;

pub use commands::build_command;
pub(crate) use labels::label_image;
pub use labels::unlabelled_reference;
pub use sandbox::{RailpackSandbox, is_pinned_image};

/// Railpack's settings for the build command, the start command, and the directory
/// of a single-page app that it serves with its own Caddy
/// (<https://railpack.com/config/environment-variables>,
/// <https://railpack.com/languages/node>).
pub(crate) const BUILD_COMMAND_SETTING: &str = "RAILPACK_BUILD_CMD";
pub(crate) const START_COMMAND_SETTING: &str = "RAILPACK_START_CMD";
pub(crate) const SPA_OUTPUT_DIRECTORY_SETTING: &str = "RAILPACK_SPA_OUTPUT_DIR";
