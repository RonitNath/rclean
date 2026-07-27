use crate::action::{Action, ActionKind, ActionReport, ActionState, Progress};
use crate::format::display_path;
use crate::path_lock::PathLocks;
use crate::proc::{command_diagnostic, run_with_timeout};
use crate::remove;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const METADATA_TIMEOUT: Duration = Duration::from_secs(60);

// Cargo keeps one of these per profile directory (and older versions kept one
// at the target root). Any of them being held means a build is live.
const BUILD_LOCK_NAMES: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

#[derive(Deserialize)]
struct CargoMetadata {
    target_directory: PathBuf,
}

pub fn execute_action(
    action: &Action,
    root: &Path,
    locks: &PathLocks,
    progress: &Progress,
    cancel: &AtomicBool,
) -> ActionReport {
    let started = Instant::now();
    let mut errors = Vec::new();

    let mut targets = Vec::new();
    for requested in requested_targets(action, &mut errors) {
        match validate_target(&requested, root) {
            Ok(target) => targets.push(target),
            Err(error) => errors.push(error),
        }
    }
    // Canonicalization collapses the two spellings of one directory.
    targets.dedup();

    let Some(primary) = targets.first().cloned() else {
        let error_count = errors.len() as u64;
        return Completion {
            path: action.path.clone(),
            errors,
            error_count,
            ..Completion::default()
        }
        .into_report(action.kind, started);
    };

    if matches!(action.kind, ActionKind::Cargo | ActionKind::Debug) {
        if let Some(lock) = targets.iter().find_map(|target| held_build_lock(target)) {
            errors.push(format!(
                "build in progress ({} is locked); left alone",
                display_path(&lock)
            ));
            // Deliberately untouched, so this is not counted as a failure.
            return Completion {
                path: primary,
                errors,
                skipped: true,
                ..Completion::default()
            }
            .into_report(action.kind, started);
        }
    }

    // Counted separately from `errors`, which holds only a capped sample.
    let mut error_count = errors.len() as u64;

    progress.set_state(ActionState::Sizing);
    let total = targets
        .iter()
        .map(|target| remove::measure_tree(target, cancel))
        .sum();
    progress.set_total(total);

    // Timed from here so measuring, and any queue wait behind an overlapping
    // action, are not reported as time spent deleting.
    let started = Instant::now();
    let mut reclaimed_bytes = 0;
    let mut files_removed = 0;
    let mut cancelled = false;

    for target in &targets {
        if locks.is_contended(target) {
            progress.set_state(ActionState::Waiting);
        }
        let guard = locks.acquire(target.clone());
        progress.set_state(ActionState::Deleting);
        let outcome = remove::remove_tree(target, progress, cancel);
        drop(guard);

        reclaimed_bytes += outcome.bytes_removed;
        files_removed += outcome.files_removed;
        error_count += outcome.error_count;
        errors.extend(outcome.errors);
        if outcome.cancelled {
            cancelled = true;
            break;
        }
    }

    if cancelled {
        errors.push("cancelled before completion".to_string());
        error_count += 1;
    }

    Completion {
        path: primary,
        errors,
        error_count,
        reclaimed_bytes,
        files_removed,
        skipped: false,
    }
    .into_report(action.kind, started)
}

/// The directories one action is responsible for reclaiming.
///
/// A cargo project is normally a single directory, but CARGO_TARGET_DIR can
/// move the live target elsewhere and leave a stale `target/` sitting in the
/// project. Both belong to this action, since discovery no longer emits a
/// separate action for a target that has a manifest above it.
fn requested_targets(action: &Action, errors: &mut Vec<String>) -> Vec<PathBuf> {
    if action.kind != ActionKind::Cargo {
        return vec![action.path.clone()];
    }

    let conventional = action.path.join("target");
    match cargo_target_directory(&action.path) {
        Ok(target) => {
            let mut targets = vec![target];
            if conventional.is_dir() {
                targets.push(conventional);
            }
            targets
        }
        Err(error) => {
            // Without metadata the conventional layout is the best guess, and
            // validate_target keeps it inside the root.
            errors.push(error);
            vec![conventional]
        }
    }
}

#[derive(Default)]
struct Completion {
    path: PathBuf,
    errors: Vec<String>,
    error_count: u64,
    reclaimed_bytes: u64,
    files_removed: u64,
    skipped: bool,
}

impl Completion {
    fn into_report(self, kind: ActionKind, started: Instant) -> ActionReport {
        ActionReport {
            kind,
            path: self.path,
            reclaimed_bytes: self.reclaimed_bytes,
            files_removed: self.files_removed,
            elapsed: started.elapsed(),
            errors: self.errors,
            error_count: self.error_count,
            skipped: self.skipped,
        }
    }
}

fn cargo_target_directory(project: &Path) -> Result<PathBuf, String> {
    let mut command = Command::new("cargo");
    command
        .current_dir(project)
        .args(["metadata", "--format-version", "1", "--no-deps"]);
    let output = run_with_timeout(command, METADATA_TIMEOUT, "cargo metadata")?;

    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            command_diagnostic(&output.stderr)
        ));
    }

    serde_json::from_slice::<CargoMetadata>(&output.stdout)
        .map(|metadata| metadata.target_directory)
        .map_err(|error| format!("could not parse cargo metadata: {error}"))
}

/// Returns the first Cargo build lock currently held under `target`.
///
/// Taking the lock ourselves would mean waiting for the build to finish, which
/// is what previously made a run hang with no output. Probing instead lets the
/// project be skipped with an explanation.
fn held_build_lock(target: &Path) -> Option<PathBuf> {
    let mut scopes = vec![target.to_path_buf()];
    // A bare `debug` action sits inside the target directory, whose root-level
    // lock also indicates a live build.
    if let Some(parent) = target.parent() {
        if parent.file_name().is_some_and(|name| name == "target") {
            scopes.push(parent.to_path_buf());
        }
    }
    if let Ok(entries) = fs::read_dir(target) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                scopes.push(entry.path());
            }
        }
    }

    scopes
        .iter()
        .flat_map(|scope| BUILD_LOCK_NAMES.iter().map(|name| scope.join(name)))
        .find(|candidate| is_locked(candidate))
}

fn is_locked(path: &Path) -> bool {
    let opened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path));
    let Ok(file) = opened else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(fs::TryLockError::WouldBlock) => true,
        Err(fs::TryLockError::Error(_)) => false,
    }
}

fn validate_target(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let canonical = canonicalize_nearest(path)?;

    if canonical == root || !canonical.starts_with(root) {
        return Err(format!(
            "refusing target outside cleanup root: {}",
            display_path(&canonical)
        ));
    }
    Ok(canonical)
}

// Canonicalize the deepest existing ancestor so the result compares cleanly
// against the canonical root (Windows canonical paths carry a \\?\ prefix)
// even when the path itself no longer exists.
fn canonicalize_nearest(path: &Path) -> Result<PathBuf, String> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            let mut canonical = fs::canonicalize(&current).map_err(|error| {
                format!("cannot canonicalize {}: {error}", display_path(&current))
            })?;
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Ok(canonical);
        }
        match (current.parent().map(Path::to_path_buf), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent;
            }
            _ => return Ok(path.to_path_buf()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_outside_the_root_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let outside = fs::canonicalize(std::env::temp_dir()).unwrap();

        let error = validate_target(&outside, &root).expect_err("must refuse");
        assert!(error.contains("refusing target"), "{error}");
        assert!(validate_target(&root, &root).is_err(), "root itself");
        assert!(validate_target(&root.join("proj/target"), &root).is_ok());
    }

    #[test]
    fn an_unlocked_cargo_lock_does_not_look_like_a_build() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target/debug");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(".cargo-lock"), b"").unwrap();

        assert!(held_build_lock(temp.path().join("target").as_path()).is_none());
    }

    #[test]
    fn a_held_cargo_lock_is_detected() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let profile = target.join("debug");
        fs::create_dir_all(&profile).unwrap();
        let lock_path = profile.join(".cargo-lock");
        fs::write(&lock_path, b"").unwrap();

        let holder = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.lock().unwrap();

        let detected = held_build_lock(&target);
        assert_eq!(detected.as_deref(), Some(lock_path.as_path()));

        let _ = holder.unlock();
        assert!(held_build_lock(&target).is_none());
    }
}
