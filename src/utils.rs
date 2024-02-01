use std::path::{Path, PathBuf};

pub fn absolute_path(path: impl AsRef<Path>) -> Result<PathBuf, anyhow::Error> {
    use path_clean::PathClean;

    let path = path.as_ref();

    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    }
    .clean();

    Ok(absolute_path)
}
