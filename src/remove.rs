use crate::action::Progress;
use crate::format::display_path;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// A single locked file can produce one failure per sibling; keep a readable
// sample rather than thousands of near-identical lines.
const MAX_REPORTED_ERRORS: usize = 20;

#[derive(Debug, Default)]
pub struct RemovalOutcome {
    pub bytes_removed: u64,
    pub files_removed: u64,
    /// Sample of failures, capped at MAX_REPORTED_ERRORS.
    pub errors: Vec<String>,
    /// Total failures, including any not present in `errors`.
    pub error_count: u64,
    pub cancelled: bool,
}

#[derive(Default)]
struct Failures {
    sample: Vec<String>,
    count: u64,
}

impl Failures {
    fn push(&mut self, message: String) {
        self.count += 1;
        if self.sample.len() < MAX_REPORTED_ERRORS {
            self.sample.push(message);
        } else if self.sample.len() == MAX_REPORTED_ERRORS {
            self.sample.push("… further failures elided".to_string());
        }
    }
}

/// Sums the apparent size of every regular file beneath `root`.
///
/// Symlinks and junctions are counted as zero and never followed, so a link
/// pointing outside the cleanup root cannot inflate the total. Unreadable
/// directories are skipped silently: the total is a progress denominator, not
/// a correctness-critical number.
pub fn measure_tree(root: &Path, cancel: &AtomicBool) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(metadata) = entry.metadata() {
                total += metadata.len();
            }
        }
    }
    total
}

enum Step {
    Enter(PathBuf),
    RemoveDirectory(PathBuf),
}

/// Deletes `root` depth-first, tolerating per-entry failures.
///
/// Unlike `fs::remove_dir_all` (and `cargo clean`, which abandons the whole
/// tree at the first error), a file that cannot be removed costs only that
/// file: everything else is still reclaimed and the failure is reported.
pub fn remove_tree(root: &Path, progress: &Progress, cancel: &AtomicBool) -> RemovalOutcome {
    let mut outcome = RemovalOutcome::default();
    let mut failures = Failures::default();

    if fs::symlink_metadata(root).is_err() {
        return outcome;
    }

    let mut stack = vec![Step::Enter(root.to_path_buf())];
    'walk: while let Some(step) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }

        match step {
            Step::Enter(directory) => {
                // Queued before the children so it pops after them.
                stack.push(Step::RemoveDirectory(directory.clone()));

                let entries = match fs::read_dir(&directory) {
                    Ok(entries) => entries,
                    Err(error) => {
                        failures.push(format!(
                            "cannot read {}: {error}",
                            display_path(&directory)
                        ));
                        continue;
                    }
                };

                for entry in entries {
                    if cancel.load(Ordering::Relaxed) {
                        outcome.cancelled = true;
                        break 'walk;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            failures.push(format!(
                                "cannot read an entry in {}: {error}",
                                display_path(&directory)
                            ));
                            continue;
                        }
                    };
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(error) => {
                            failures
                                .push(format!("cannot inspect {}: {error}", display_path(&path)));
                            continue;
                        }
                    };

                    if file_type.is_symlink() {
                        match remove_symlink(&path, &file_type) {
                            Ok(()) => {
                                outcome.files_removed += 1;
                                progress.record_removed(0);
                            }
                            Err(error) => failures.push(format!(
                                "cannot remove link {}: {error}",
                                display_path(&path)
                            )),
                        }
                        continue;
                    }

                    if file_type.is_dir() {
                        stack.push(Step::Enter(path));
                        continue;
                    }

                    let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                    match remove_file(&path) {
                        Ok(()) => {
                            outcome.bytes_removed += size;
                            outcome.files_removed += 1;
                            progress.record_removed(size);
                        }
                        Err(error) => failures
                            .push(format!("cannot remove {}: {error}", display_path(&path))),
                    }
                }
            }

            Step::RemoveDirectory(directory) => {
                if let Err(error) = remove_directory(&directory) {
                    // "Not empty" only ever means a child already failed and
                    // was reported; repeating it for every ancestor is noise.
                    if error.kind() != io::ErrorKind::DirectoryNotEmpty {
                        failures.push(format!(
                            "cannot remove directory {}: {error}",
                            display_path(&directory)
                        ));
                    }
                }
            }
        }
    }

    outcome.errors = failures.sample;
    outcome.error_count = failures.count;
    outcome
}

fn remove_file(path: &Path) -> io::Result<()> {
    remove_with_readonly_retry(path, |path| fs::remove_file(path))
}

fn remove_directory(path: &Path) -> io::Result<()> {
    remove_with_readonly_retry(path, |path| fs::remove_dir(path))
}

// Windows refuses to unlink read-only entries, which npm and some build tools
// leave behind; clearing the attribute and retrying rescues those.
fn remove_with_readonly_retry(
    path: &Path,
    remove: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match remove(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            if !clear_readonly(path) {
                return Err(error);
            }
            remove(path)
        }
    }
}

fn clear_readonly(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    let mut permissions = metadata.permissions();
    if !permissions.readonly() {
        return false;
    }
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).is_ok()
}

#[cfg(windows)]
fn remove_symlink(path: &Path, file_type: &fs::FileType) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    // Directory symlinks and junctions are reparse points that must be
    // unlinked with remove_dir; remove_file fails on them.
    if file_type.is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(not(windows))]
fn remove_symlink(path: &Path, _file_type: &fs::FileType) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn write_file(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(&vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn measures_only_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(&root.join("a.bin"), 1000);
        write_file(&root.join("nested/b.bin"), 2000);
        fs::create_dir_all(root.join("empty")).unwrap();

        assert_eq!(measure_tree(root, &no_cancel()), 3000);
    }

    #[test]
    fn removes_whole_tree_and_reports_exact_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target");
        write_file(&root.join("a.bin"), 1500);
        write_file(&root.join("deep/nested/b.bin"), 2500);

        let progress = Progress::default();
        let outcome = remove_tree(&root, &progress, &no_cancel());

        assert_eq!(outcome.error_count, 0, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.bytes_removed, 4000);
        assert_eq!(outcome.files_removed, 2);
        assert_eq!(progress.done_bytes(), 4000);
        assert!(!root.exists());
    }

    #[test]
    fn read_only_files_are_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target");
        let locked_down = root.join("readonly.bin");
        write_file(&locked_down, 128);
        let mut permissions = fs::metadata(&locked_down).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&locked_down, permissions).unwrap();

        let outcome = remove_tree(&root, &Progress::default(), &no_cancel());

        assert_eq!(outcome.error_count, 0, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.bytes_removed, 128);
        assert!(!root.exists());
    }

    // The dentconnex scenario: one undeletable file must not cost the tree.
    #[test]
    fn one_locked_file_does_not_abandon_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target");
        write_file(&root.join("keep_me_open.bin"), 400);
        write_file(&root.join("a.bin"), 1000);
        write_file(&root.join("deep/nested/b.bin"), 2000);

        let held = open_exclusive(&root.join("keep_me_open.bin"));
        let outcome = remove_tree(&root, &Progress::default(), &no_cancel());
        drop(held);

        if outcome.error_count == 0 {
            // Platforms with advisory-only unlink semantics delete it anyway.
            assert_eq!(outcome.bytes_removed, 3400);
            return;
        }
        assert_eq!(outcome.error_count, 1, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.bytes_removed, 3000, "everything else is reclaimed");
        assert!(!root.join("a.bin").exists());
        assert!(!root.join("deep").exists());
        assert!(root.join("keep_me_open.bin").exists());
    }

    #[cfg(windows)]
    fn open_exclusive(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0) // deny all sharing, so unlink fails
            .open(path)
            .unwrap()
    }

    #[cfg(not(windows))]
    fn open_exclusive(path: &Path) -> File {
        File::open(path).unwrap()
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target");
        for index in 0..50 {
            write_file(&root.join(format!("file_{index}.bin")), 10);
        }

        let cancel = AtomicBool::new(true);
        let outcome = remove_tree(&root, &Progress::default(), &cancel);

        assert!(outcome.cancelled);
        assert_eq!(outcome.files_removed, 0);
        assert!(root.exists());
    }

    #[test]
    fn missing_target_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let outcome = remove_tree(
            &temp.path().join("absent"),
            &Progress::default(),
            &no_cancel(),
        );
        assert_eq!(outcome.error_count, 0);
        assert_eq!(outcome.bytes_removed, 0);
    }
}
