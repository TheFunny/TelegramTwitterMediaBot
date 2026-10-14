pub mod media;
pub mod site;

/// Prefix every temp file and temp dir this project creates, so a startup
/// sweep can recognise its own leftovers: a killed process leaves them behind
/// (`TempDir`/`NamedTempFile` clean up on drop, and a killed process runs no
/// destructors), and without a marker the only safe assumption about the OS
/// temp directory is "not mine".
pub const TEMP_FILE_PREFIX: &str = "tgxmb-";

/// True when `path` is a file inside a temp directory this project created: the
/// directory's name carries [`TEMP_FILE_PREFIX`] and it sits directly under the
/// OS temp directory (`tempfile`'s default location). The prefix lives on the
/// directory, not the file, so checking the file name is not enough; requiring
/// the parent to be the temp root stops an upstream-supplied bare path that
/// merely contains a `tgxmb-` component from passing.
pub fn is_own_temp_file(path: &std::path::Path) -> bool {
    let Some(dir) = path.parent().and_then(|dir| dir.canonicalize().ok()) else {
        return false;
    };
    let Ok(root) = std::env::temp_dir().canonicalize() else {
        return false;
    };
    dir.parent() == Some(root.as_path())
        && dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(TEMP_FILE_PREFIX))
}
