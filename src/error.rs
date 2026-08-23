use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct FileError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct SkipLog {
    errors: Vec<FileError>,
}

impl SkipLog {
    pub fn push(&mut self, path: PathBuf, message: impl Into<String>) {
        self.errors.push(FileError {
            path,
            message: message.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &FileError> {
        self.errors.iter()
    }

    pub fn print_summary(&self) {
        if self.errors.is_empty() {
            return;
        }
        eprintln!(
            "\n{} file(s) had errors and were skipped:",
            self.errors.len()
        );
        for e in &self.errors {
            eprintln!("  {}: {}", e.path.display(), e.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_empty_on_new_log() {
        assert!(SkipLog::default().is_empty());
    }

    #[test]
    fn test_push_makes_log_non_empty() {
        let mut log = SkipLog::default();
        log.push(PathBuf::from("/some/file.txt"), "permission denied");
        assert!(!log.is_empty());
    }

    #[test]
    fn test_print_summary_empty_does_not_panic() {
        SkipLog::default().print_summary();
    }

    #[test]
    fn test_print_summary_with_entries_does_not_panic() {
        let mut log = SkipLog::default();
        log.push(PathBuf::from("/a/b.txt"), "no space left on device");
        log.push(PathBuf::from("/c/d.txt"), "permission denied");
        log.print_summary();
    }
}
