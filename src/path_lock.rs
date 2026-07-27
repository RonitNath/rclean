use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

// Serializes actions whose paths nest inside one another (a cargo target and
// the target/debug directory discovered separately) so two workers never
// delete the same subtree at once.
#[derive(Default)]
pub struct PathLocks {
    active: Mutex<Vec<PathBuf>>,
    changed: Condvar,
}

pub struct PathLockGuard<'a> {
    locks: &'a PathLocks,
    path: PathBuf,
}

impl PathLocks {
    pub fn is_contended(&self, path: &Path) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        active.iter().any(|other| paths_overlap(path, other))
    }

    pub fn acquire(&self, path: PathBuf) -> PathLockGuard<'_> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while active.iter().any(|other| paths_overlap(&path, other)) {
            active = self
                .changed
                .wait(active)
                .unwrap_or_else(|error| error.into_inner());
        }
        active.push(path.clone());
        PathLockGuard { locks: self, path }
    }
}

impl Drop for PathLockGuard<'_> {
    fn drop(&mut self) {
        let mut active = self
            .locks
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        active.retain(|path| path != &self.path);
        self.locks.changed.notify_all();
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_paths_overlap_but_siblings_do_not() {
        assert!(paths_overlap(Path::new("/a/b"), Path::new("/a")));
        assert!(paths_overlap(Path::new("/a"), Path::new("/a/b")));
        assert!(!paths_overlap(Path::new("/a/b"), Path::new("/a/c")));
    }

    #[test]
    fn guard_release_allows_a_later_acquire() {
        let locks = PathLocks::default();
        {
            let _guard = locks.acquire(PathBuf::from("/a"));
            assert!(locks.is_contended(Path::new("/a/b")));
        }
        assert!(!locks.is_contended(Path::new("/a/b")));
    }
}
