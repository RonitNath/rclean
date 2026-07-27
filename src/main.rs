mod action;
mod discover;
mod execute;
mod format;
mod path_lock;
mod proc;
mod remove;
mod ui;

use action::{ActionReport, ActionState, Progress};
use format::{human_duration, human_size};
use path_lock::PathLocks;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Instant;
use threadpool::ThreadPool;

const EXIT_FAILURE: i32 = 1;
const EXIT_CANCELLED: i32 = 130;

fn main() {
    let run_started = Instant::now();
    let root = match cleanup_root() {
        Ok(root) => Arc::new(root),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(EXIT_FAILURE);
        }
    };

    eprintln!("Scanning {}", format::display_path(&root));
    let (actions, discovery_errors) = discover::discover_actions(&root);

    let progress: Vec<Arc<Progress>> = actions
        .iter()
        .map(|_| Arc::new(Progress::default()))
        .collect();
    let cancel = Arc::new(AtomicBool::new(false));
    let locks = Arc::new(PathLocks::default());
    let (sender, receiver) = mpsc::channel();

    let pool = ThreadPool::new(worker_count());
    for (index, action) in actions.iter().enumerate() {
        let action = action.clone();
        let sender = sender.clone();
        let root = Arc::clone(&root);
        let locks = Arc::clone(&locks);
        let cancel = Arc::clone(&cancel);
        let progress = Arc::clone(&progress[index]);
        pool.execute(move || {
            let report = execute::execute_action(&action, &root, &locks, &progress, &cancel);
            progress.set_state(ActionState::Done);
            let _ = sender.send((index, report));
        });
    }
    // The UI loop ends when the last report lands, so no stray sender may live
    // past this point.
    drop(sender);

    let outcome = ui::run(&ui::RunContext {
        root: &root,
        actions: &actions,
        progress: &progress,
        receiver: &receiver,
        discovery_errors: &discovery_errors,
        cancel: &cancel,
        started: run_started,
    });

    pool.join();

    let mut reports = outcome.reports;
    while let Ok(pair) = receiver.try_recv() {
        reports.push(pair);
    }
    reports.sort_by_key(|(index, _)| *index);
    let reports: Vec<ActionReport> = reports.into_iter().map(|(_, report)| report).collect();

    // The dashboard painted over the scrollback, so the durable record of what
    // happened is printed only after the terminal is restored.
    if !outcome.already_logged {
        for error in &discovery_errors {
            eprintln!("[discovery error] {error}");
        }
        for report in &reports {
            ui::print_report(report);
        }
    }

    let failures = report_summary(
        &reports,
        actions.len(),
        discovery_errors.len(),
        run_started,
    );

    if outcome.cancelled {
        std::process::exit(EXIT_CANCELLED);
    }
    if failures > 0 {
        std::process::exit(EXIT_FAILURE);
    }
}

fn report_summary(
    reports: &[ActionReport],
    action_count: usize,
    discovery_error_count: usize,
    run_started: Instant,
) -> u64 {
    let reclaimed_bytes: u64 = reports.iter().map(|report| report.reclaimed_bytes).sum();
    let files_removed: u64 = reports.iter().map(|report| report.files_removed).sum();
    let productive = reports
        .iter()
        .filter(|report| report.reclaimed_bytes > 0)
        .count();
    let skipped = reports.iter().filter(|report| report.skipped).count();
    let failures = reports
        .iter()
        .map(|report| report.error_count)
        .sum::<u64>()
        + discovery_error_count as u64
        + action_count.saturating_sub(reports.len()) as u64;

    let mut line = format!(
        "Complete: {productive} of {} actions reclaimed space, {} across {}, {}, {} total",
        reports.len(),
        human_size(reclaimed_bytes),
        plural(files_removed, "file"),
        plural(failures, "problem"),
        human_duration(run_started.elapsed())
    );
    if skipped > 0 {
        line.push_str(&format!(" ({skipped} skipped: build running)"));
    }
    println!("{line}");

    failures
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn cleanup_root() -> Result<PathBuf, String> {
    let current = std::env::current_dir()
        .map_err(|error| format!("Could not get current directory: {error}"))?;
    let parent = current
        .parent()
        .ok_or_else(|| "Current directory has no parent".to_string())?;
    fs::canonicalize(parent).map_err(|error| {
        format!(
            "Could not canonicalize cleanup root {}: {error}",
            parent.display()
        )
    })
}

fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
}
