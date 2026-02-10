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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_already_absolute() {
        let result = absolute_path("/usr/bin/foo").unwrap();
        assert_eq!(result, PathBuf::from("/usr/bin/foo"));
    }

    #[test]
    fn absolute_path_cleans_dot_dot() {
        let result = absolute_path("/usr/bin/../lib/foo").unwrap();
        assert_eq!(result, PathBuf::from("/usr/lib/foo"));
    }

    #[test]
    fn absolute_path_cleans_dot() {
        let result = absolute_path("/usr/./bin/foo").unwrap();
        assert_eq!(result, PathBuf::from("/usr/bin/foo"));
    }

    #[test]
    fn absolute_path_relative_prepends_cwd() {
        let result = absolute_path("some/relative/path").unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(result, cwd.join("some/relative/path"));
        assert!(result.is_absolute());
    }

    #[test]
    fn absolute_path_relative_with_dot_dot() {
        let result = absolute_path("foo/../bar").unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(result, cwd.join("bar"));
    }
}
