use crate::action::{Action, ActionKind};
use crate::format::display_path;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn direct_action_kind(name: &str) -> Option<ActionKind> {
    match name {
        "debug" => Some(ActionKind::Debug),
        "node_modules" => Some(ActionKind::NodeModules),
        ".venv" => Some(ActionKind::PythonVenv),
        "scratch" | ".scratch" => Some(ActionKind::Scratch),
        _ => None,
    }
}

pub fn discover_actions(root: &Path) -> (Vec<Action>, Vec<String>) {
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
    walk(
        root,
        root,
        &mut visited,
        &mut actions,
        &mut errors,
        root_is_cargo,
    );

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
            errors.push(format!("cannot read {}: {error}", display_path(directory)));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!(
                    "cannot read an entry in {}: {error}",
                    display_path(directory)
                ));
                continue;
            }
        };

        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors.push(format!("cannot inspect {}: {error}", display_path(&path)));
                continue;
            }
        };

        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }

        let canonical = match fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            Err(error) => {
                errors.push(format!(
                    "cannot canonicalize {}: {error}",
                    display_path(&path)
                ));
                continue;
            }
        };

        if canonical == root || !canonical.starts_with(root) {
            errors.push(format!(
                "refusing directory outside cleanup root: {}",
                display_path(&canonical)
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

        // Avoid walking millions of build artifacts. A target belonging to a
        // manifest is already covered by that project's cargo action, so only
        // an orphaned one — a vendored or copied tree with no manifest above
        // it — needs an action of its own.
        if name == "target" {
            if inside_cargo {
                continue;
            }
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
                        display_path(&debug)
                    )),
                    Err(error) => errors.push(format!(
                        "cannot canonicalize {}: {error}",
                        display_path(&debug)
                    )),
                }
            }
            continue;
        }

        // Workspace members share the workspace-level target directory, so one
        // action at the outermost manifest covers everything below.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    fn kinds_for(actions: &[Action], kind: ActionKind) -> Vec<PathBuf> {
        let mut paths: Vec<_> = actions
            .iter()
            .filter(|action| action.kind == kind)
            .map(|action| action.path.clone())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn finds_each_reclaimable_directory_kind() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        touch(&root.join("web/node_modules/pkg/index.js"));
        touch(&root.join("py/.venv/lib/site.py"));
        fs::create_dir_all(root.join("scratch")).unwrap();
        touch(&root.join("rs/Cargo.toml"));
        touch(&root.join("rs/target/debug/app.exe"));

        let (actions, errors) = discover_actions(&root);

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(
            kinds_for(&actions, ActionKind::NodeModules),
            vec![root.join("web/node_modules")]
        );
        assert_eq!(
            kinds_for(&actions, ActionKind::PythonVenv),
            vec![root.join("py/.venv")]
        );
        assert_eq!(
            kinds_for(&actions, ActionKind::Scratch),
            vec![root.join("scratch")]
        );
        assert_eq!(kinds_for(&actions, ActionKind::Cargo), vec![root.join("rs")]);
        assert!(
            kinds_for(&actions, ActionKind::Debug).is_empty(),
            "the cargo action already covers this project's target"
        );
    }

    // Without a manifest above it there is no cargo action to clean this, so
    // it needs one of its own.
    #[test]
    fn an_orphaned_target_debug_gets_its_own_action() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        touch(&root.join("vendored/target/debug/leftover.rlib"));

        let (actions, _) = discover_actions(&root);

        assert_eq!(
            kinds_for(&actions, ActionKind::Debug),
            vec![root.join("vendored/target/debug")]
        );
        assert!(kinds_for(&actions, ActionKind::Cargo).is_empty());
    }

    // Workspace members share the root target dir, so only the outer manifest
    // should become an action.
    #[test]
    fn nested_manifests_collapse_to_the_outermost() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        touch(&root.join("ws/Cargo.toml"));
        touch(&root.join("ws/crates/inner/Cargo.toml"));

        let (actions, _) = discover_actions(&root);

        assert_eq!(kinds_for(&actions, ActionKind::Cargo), vec![root.join("ws")]);
    }

    #[test]
    fn target_contents_are_not_traversed() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        touch(&root.join("rs/Cargo.toml"));
        // A node_modules buried inside target must not become its own action;
        // deleting the target already covers it.
        touch(&root.join("rs/target/debug/build/x/node_modules/a.js"));

        let (actions, _) = discover_actions(&root);

        assert!(kinds_for(&actions, ActionKind::NodeModules).is_empty());
    }

    #[test]
    fn git_directories_are_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        touch(&root.join("proj/.git/objects/ab/cdef"));
        touch(&root.join("proj/.git/node_modules/should_not_match.js"));

        let (actions, _) = discover_actions(&root);

        assert!(actions.is_empty(), "unexpected actions: {actions:?}");
    }
}
