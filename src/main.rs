use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use threadpool::ThreadPool;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ActionKind {
    Cargo,
    Debug,
    NodeModules,
    PythonVenv,
    Scratch,
}

impl ActionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Debug => "debug",
            Self::NodeModules => "node_modules",
            Self::PythonVenv => ".venv",
            Self::Scratch => "scratch",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Action {
    kind: ActionKind,
    path: PathBuf,
}

#[derive(Debug)]
struct ActionReport {
    kind: ActionKind,
    path: PathBuf,
    reclaimed_kib: Option<u64>,
    elapsed: Duration,
    errors: Vec<String>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    target_directory: PathBuf,
}

#[derive(Default)]
struct PathLocks {
    active: Mutex<Vec<PathBuf>>,
    changed: Condvar,
}

struct PathLockGuard<'a> {
    locks: &'a PathLocks,
    path: PathBuf,
}

impl PathLocks {
    fn acquire(&self, path: PathBuf) -> PathLockGuard<'_> {
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

fn direct_action_kind(name: &str) -> Option<ActionKind> {
    match name {
        "debug" => Some(ActionKind::Debug),
        "node_modules" => Some(ActionKind::NodeModules),
        ".venv" => Some(ActionKind::PythonVenv),
        "scratch" | ".scratch" => Some(ActionKind::Scratch),
        _ => None,
    }
}

fn discover_actions(root: &Path) -> (Vec<Action>, Vec<String>) {
    let mut actions = HashSet::new();
    let mut visited = HashSet::from([root.to_path_buf()]);
    let mut errors = Vec::new();
    let root_is_cargo = root.join("Cargo.toml").is_file();
    if root_is_cargo {
        actions.insert(Action {
            kind: ActionKind::Cargo,
            path: root.to_path_buf(),
        });
    }
    walk(root, root, &mut visited, &mut actions, &mut errors, root_is_cargo);

    let mut actions: Vec<_> = actions.into_iter().collect();
    actions.sort_by(|left, right| left.path.cmp(&right.path));
    (actions, errors)
}

fn walk(
    directory: &Path,
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    actions: &mut HashSet<Action>,
    errors: &mut Vec<String>,
    inside_cargo: bool,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", directory.display()));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!(
                    "cannot read an entry in {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };

        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors.push(format!("cannot inspect {}: {error}", path.display()));
                continue;
            }
        };

        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }

        let canonical = match fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            Err(error) => {
                errors.push(format!("cannot canonicalize {}: {error}", path.display()));
                continue;
            }
        };

        if canonical == root || !canonical.starts_with(root) {
            errors.push(format!(
                "refusing directory outside cleanup root: {}",
                canonical.display()
            ));
            continue;
        }

        if !visited.insert(canonical.clone()) {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();

        if let Some(kind) = direct_action_kind(&name) {
            actions.insert(Action {
                kind,
                path: canonical,
            });
            continue;
        }

        if name == ".git" {
            continue;
        }

        // Avoid walking millions of build artifacts, but still make the debug
        // directory independently removable when Cargo metadata or cleaning
        // does not work.
        if name == "target" {
            let debug = canonical.join("debug");
            let is_real_directory = fs::symlink_metadata(&debug)
                .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                .unwrap_or(false);
            if is_real_directory {
                match fs::canonicalize(&debug) {
                    Ok(debug) if debug != root && debug.starts_with(root) => {
                        actions.insert(Action {
                            kind: ActionKind::Debug,
                            path: debug,
                        });
                    }
                    Ok(debug) => errors.push(format!(
                        "refusing debug directory outside cleanup root: {}",
                        debug.display()
                    )),
                    Err(error) => {
                        errors.push(format!("cannot canonicalize {}: {error}", debug.display()))
                    }
                }
            }
            continue;
        }

        // Workspace members share the workspace-level target directory, so
        // one cargo clean at the outermost manifest covers everything below.
        let has_manifest = canonical.join("Cargo.toml").is_file();
        if has_manifest && !inside_cargo {
            actions.insert(Action {
                kind: ActionKind::Cargo,
                path: canonical.clone(),
            });
        }

        walk(
            &canonical,
            root,
            visited,
            actions,
            errors,
            inside_cargo || has_manifest,
        );
    }
}

#[cfg(windows)]
fn external_command_path(path: &Path) -> PathBuf {
    let path = path.to_string_lossy();
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{path}"))
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        PathBuf::from(path)
    } else {
        PathBuf::from(path.as_ref())
    }
}

#[cfg(not(windows))]
fn external_command_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn du_kib(path: &Path) -> Result<u64, String> {
    if !path.exists() {
        return Ok(0);
    }

    let output = Command::new("du")
        .arg("-sk")
        .arg("--")
        .arg(external_command_path(path))
        .output()
        .map_err(|error| format!("could not run du for {}: {error}", path.display()))?;

    if !output.status.success() {
        return Err(format!(
            "du failed for {}: {}",
            path.display(),
            command_diagnostic(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("du returned no size for {}", path.display()))?
        .parse::<u64>()
        .map_err(|error| format!("could not parse du output for {}: {error}", path.display()))
}

fn command_diagnostic(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if message.is_empty() {
        "no diagnostic output".to_string()
    } else {
        message
    }
}

fn remove_directory(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path)
        .map_err(|error| format!("could not remove {}: {error}", path.display()))
}

fn cargo_target_directory(project: &Path) -> Result<PathBuf, String> {
    let output = Command::new("cargo")
        .current_dir(project)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|error| format!("could not run cargo metadata: {error}"))?;

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

fn validate_target(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let canonical = canonicalize_nearest(path)?;

    if canonical == root || !canonical.starts_with(root) {
        return Err(format!(
            "refusing target outside cleanup root: {}",
            canonical.display()
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
            let mut canonical = fs::canonicalize(&current)
                .map_err(|error| format!("cannot canonicalize {}: {error}", current.display()))?;
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

fn execute_direct(action: Action, root: &Path, locks: &PathLocks) -> ActionReport {
    let started = Instant::now();
    let mut errors = Vec::new();

    let target = match validate_target(&action.path, root) {
        Ok(target) => target,
        Err(error) => {
            return ActionReport {
                kind: action.kind,
                path: action.path,
                reclaimed_kib: None,
                elapsed: started.elapsed(),
                errors: vec![error],
            };
        }
    };

    let _guard = locks.acquire(target.clone());
    let started = Instant::now();
    let before = du_kib(&target).map_err(|error| errors.push(error)).ok();

    if let Err(error) = remove_directory(&target) {
        errors.push(error);
    }

    let after = du_kib(&target).map_err(|error| errors.push(error)).ok();
    ActionReport {
        kind: action.kind,
        path: target,
        reclaimed_kib: before
            .zip(after)
            .map(|(before, after)| before.saturating_sub(after)),
        elapsed: started.elapsed(),
        errors,
    }
}

fn execute_cargo(project: PathBuf, root: &Path, locks: &PathLocks) -> ActionReport {
    let initial_started = Instant::now();
    let mut errors = Vec::new();
    let (raw_target, metadata_available) = match cargo_target_directory(&project) {
        Ok(target) => (target, true),
        Err(error) => {
            errors.push(error);
            (project.join("target"), false)
        }
    };

    let target = match validate_target(&raw_target, root) {
        Ok(target) => target,
        Err(error) => {
            errors.push(error);
            return ActionReport {
                kind: ActionKind::Cargo,
                path: project,
                reclaimed_kib: None,
                elapsed: initial_started.elapsed(),
                errors,
            };
        }
    };

    let _guard = locks.acquire(target.clone());
    let started = Instant::now();
    let before = du_kib(&target).map_err(|error| errors.push(error)).ok();

    let cargo_failed = if metadata_available {
        let output = Command::new("cargo")
            .current_dir(&project)
            .arg("clean")
            .output();

        match output {
            Ok(output) if output.status.success() => false,
            Ok(output) => {
                errors.push(format!(
                    "cargo clean failed in {}: {}",
                    project.display(),
                    command_diagnostic(&output.stderr)
                ));
                true
            }
            Err(error) => {
                errors.push(format!(
                    "could not run cargo clean in {}: {error}",
                    project.display()
                ));
                true
            }
        }
    } else {
        true
    };

    if cargo_failed {
        let fallback = target.join("debug");
        if let Err(error) = remove_directory(&fallback) {
            errors.push(format!("debug fallback failed: {error}"));
        }
    }

    let after = du_kib(&target).map_err(|error| errors.push(error)).ok();
    ActionReport {
        kind: ActionKind::Cargo,
        path: target,
        reclaimed_kib: before
            .zip(after)
            .map(|(before, after)| before.saturating_sub(after)),
        elapsed: started.elapsed(),
        errors,
    }
}

fn execute_action(action: Action, root: &Path, locks: &PathLocks) -> ActionReport {
    if action.kind == ActionKind::Cargo {
        execute_cargo(action.path, root, locks)
    } else {
        execute_direct(action, root, locks)
    }
}

fn human_size(kib: u64) -> String {
    let bytes = kib as f64 * 1024.0;
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn human_duration(duration: Duration) -> String {
    if duration.as_secs() >= 60 {
        format!("{:.1}m", duration.as_secs_f64() / 60.0)
    } else if duration.as_secs() >= 1 {
        format!("{:.2}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn print_report(report: &ActionReport) {
    let status = if report.errors.is_empty() {
        "ok"
    } else {
        "error"
    };
    let reclaimed = report
        .reclaimed_kib
        .map(human_size)
        .unwrap_or_else(|| "unknown space".to_string());
    println!(
        "[{status}] {:<12} {} — {} reclaimed in {}",
        report.kind.label(),
        report.path.display(),
        reclaimed,
        human_duration(report.elapsed)
    );
    for error in &report.errors {
        eprintln!("  {error}");
    }
}

fn main() {
    let run_started = Instant::now();
    let current_directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("Could not get current directory: {error}");
            std::process::exit(1);
        }
    };
    let parent = match current_directory.parent() {
        Some(parent) => parent,
        None => {
            eprintln!("Current directory has no parent");
            std::process::exit(1);
        }
    };
    let root = match fs::canonicalize(parent) {
        Ok(root) => Arc::new(root),
        Err(error) => {
            eprintln!(
                "Could not canonicalize cleanup root {}: {error}",
                parent.display()
            );
            std::process::exit(1);
        }
    };

    println!("Scanning {}", root.display());
    let (actions, discovery_errors) = discover_actions(&root);
    for error in &discovery_errors {
        eprintln!("[discovery error] {error}");
    }
    println!("Running {} cleanup actions", actions.len());

    let pool = ThreadPool::new(num_cpus::get().max(1));
    let locks = Arc::new(PathLocks::default());
    let (sender, receiver) = mpsc::channel();
    let action_count = actions.len();

    for action in actions {
        let sender = sender.clone();
        let root = Arc::clone(&root);
        let locks = Arc::clone(&locks);
        pool.execute(move || {
            let report = execute_action(action, &root, &locks);
            let _ = sender.send(report);
        });
    }
    drop(sender);

    let mut reports = Vec::with_capacity(action_count);
    for report in receiver {
        print_report(&report);
        reports.push(report);
    }
    pool.join();

    let reclaimed_kib = reports
        .iter()
        .filter_map(|report| report.reclaimed_kib)
        .sum();
    let failed = reports
        .iter()
        .map(|report| report.errors.len())
        .sum::<usize>()
        + discovery_errors.len()
        + action_count.saturating_sub(reports.len());

    println!(
        "Complete: {} actions, {} reclaimed, {} errors, {} total",
        reports.len(),
        human_size(reclaimed_kib),
        failed,
        human_duration(run_started.elapsed())
    );

    if failed > 0 {
        std::process::exit(1);
    }
}
