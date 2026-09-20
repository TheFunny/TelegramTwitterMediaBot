pub mod media;
pub mod site;

/// Prefix every temp file and temp dir this project creates, so a startup
/// sweep can recognise its own leftovers: a killed process leaves them behind
/// (`TempDir`/`NamedTempFile` clean up on drop, and a killed process runs no
/// destructors), and without a marker the only safe assumption about the OS
/// temp directory is "not mine".
pub const TEMP_FILE_PREFIX: &str = "tgxmb-";
