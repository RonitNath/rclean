use crate::action::{Action, ActionReport, ActionState, Progress};
use crate::format::{display_path, human_duration, human_rate, human_size, relative_display};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Gauge, List, ListItem, Paragraph, Row, Table, TableState};
use ratatui::{Frame, DefaultTerminal};
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const BAR_WIDTH: usize = 10;

pub struct RunContext<'a> {
    pub root: &'a Path,
    pub actions: &'a [Action],
    pub progress: &'a [Arc<Progress>],
    pub receiver: &'a Receiver<(usize, ActionReport)>,
    pub discovery_errors: &'a [String],
    pub cancel: &'a AtomicBool,
    pub started: Instant,
}

pub struct UiOutcome {
    pub reports: Vec<(usize, ActionReport)>,
    pub cancelled: bool,
    /// Plain mode streams per-action lines as they land; the dashboard paints
    /// over the screen and leaves reprinting to the caller.
    pub already_logged: bool,
}

pub fn run(context: &RunContext) -> UiOutcome {
    if std::io::stdout().is_terminal() {
        match ratatui::try_init() {
            Ok(terminal) => return run_dashboard(context, terminal),
            // A terminal we cannot drive is not worth failing the cleanup over.
            Err(error) => eprintln!("Falling back to plain output: {error}"),
        }
    }
    run_plain(context)
}

fn run_plain(context: &RunContext) -> UiOutcome {
    for error in context.discovery_errors {
        eprintln!("[discovery error] {error}");
    }
    println!("Running {} cleanup actions", context.actions.len());

    let mut reports = Vec::with_capacity(context.actions.len());
    let mut announced = vec![false; context.actions.len()];

    while reports.len() < context.actions.len() {
        announce_started(context, &mut announced);

        match context.receiver.recv_timeout(FRAME_INTERVAL) {
            Ok((index, report)) => {
                // An action fast enough to finish inside one poll window has
                // not been announced yet; do it now so starts precede results.
                announce_started(context, &mut announced);
                print_report(&report);
                reports.push((index, report));
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    UiOutcome {
        reports,
        cancelled: false,
        already_logged: true,
    }
}

fn announce_started(context: &RunContext, announced: &mut [bool]) {
    for (index, progress) in context.progress.iter().enumerate() {
        if !announced[index] && progress.state() != ActionState::Pending {
            announced[index] = true;
            println!(
                "[start] {:<12} {}",
                context.actions[index].kind.label(),
                display_path(&context.actions[index].path)
            );
        }
    }
}

pub fn print_report(report: &ActionReport) {
    let status = if report.skipped {
        "skip"
    } else if report.error_count > 0 {
        "error"
    } else {
        "ok"
    };
    println!(
        "[{status}] {:<12} {} — {} reclaimed in {}",
        report.kind.label(),
        display_path(&report.path),
        human_size(report.reclaimed_bytes),
        human_duration(report.elapsed)
    );
    for error in &report.errors {
        eprintln!("  {error}");
    }
}

struct Finished {
    reclaimed_bytes: u64,
    elapsed: Duration,
    error_count: u64,
    skipped: bool,
}

// Smooths the byte rate so the footer does not flicker between frames.
struct RateTracker {
    sampled_at: Instant,
    sampled_bytes: u64,
    rate: f64,
}

impl RateTracker {
    fn new(started: Instant) -> Self {
        Self {
            sampled_at: started,
            sampled_bytes: 0,
            rate: 0.0,
        }
    }

    fn sample(&mut self, bytes: u64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.sampled_at).as_secs_f64();
        if elapsed < 0.5 {
            return;
        }
        let instant = bytes.saturating_sub(self.sampled_bytes) as f64 / elapsed;
        self.rate = if self.rate == 0.0 {
            instant
        } else {
            self.rate * 0.7 + instant * 0.3
        };
        self.sampled_at = now;
        self.sampled_bytes = bytes;
    }
}

struct Dashboard {
    finished: Vec<Option<Finished>>,
    errors: Vec<String>,
    action_offset: usize,
    follow_active: bool,
    error_offset: usize,
    rate: RateTracker,
    cancelling: bool,
}

impl Dashboard {
    fn new(context: &RunContext) -> Self {
        Self {
            finished: (0..context.actions.len()).map(|_| None).collect(),
            errors: context.discovery_errors.to_vec(),
            action_offset: 0,
            follow_active: true,
            error_offset: 0,
            rate: RateTracker::new(context.started),
            cancelling: false,
        }
    }

    fn record(&mut self, index: usize, report: &ActionReport, root: &Path) {
        self.finished[index] = Some(Finished {
            reclaimed_bytes: report.reclaimed_bytes,
            elapsed: report.elapsed,
            error_count: report.error_count,
            skipped: report.skipped,
        });
        let label = relative_display(&report.path, root);
        for error in &report.errors {
            self.errors.push(format!("{label}: {error}"));
        }
    }
}

fn run_dashboard(context: &RunContext, mut terminal: DefaultTerminal) -> UiOutcome {
    let mut dashboard = Dashboard::new(context);
    let mut reports = Vec::with_capacity(context.actions.len());
    let mut cancelled = false;
    let mut disconnected = false;

    loop {
        loop {
            match context.receiver.try_recv() {
                Ok((index, report)) => {
                    dashboard.record(index, &report, context.root);
                    reports.push((index, report));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        let _ = terminal.draw(|frame| draw(frame, context, &mut dashboard));

        if reports.len() >= context.actions.len() || disconnected {
            break;
        }

        if let Some(key) = poll_key() {
            match key {
                Key::Quit => {
                    cancelled = true;
                    dashboard.cancelling = true;
                    context.cancel.store(true, Ordering::Relaxed);
                }
                Key::ActionsUp => {
                    dashboard.follow_active = false;
                    dashboard.action_offset = dashboard.action_offset.saturating_sub(1);
                }
                Key::ActionsDown => {
                    dashboard.follow_active = false;
                    dashboard.action_offset = (dashboard.action_offset + 1)
                        .min(context.actions.len().saturating_sub(1));
                }
                Key::ActionsFollow => dashboard.follow_active = true,
                Key::ErrorsUp => {
                    dashboard.error_offset = dashboard.error_offset.saturating_sub(1)
                }
                Key::ErrorsDown => {
                    dashboard.error_offset = (dashboard.error_offset + 1)
                        .min(dashboard.errors.len().saturating_sub(1))
                }
            }
        }
    }

    ratatui::restore();
    UiOutcome {
        reports,
        cancelled,
        already_logged: false,
    }
}

enum Key {
    Quit,
    ActionsUp,
    ActionsDown,
    ActionsFollow,
    ErrorsUp,
    ErrorsDown,
}

fn poll_key() -> Option<Key> {
    if !event::poll(FRAME_INTERVAL).unwrap_or(false) {
        return None;
    }
    let Ok(Event::Key(key)) = event::read() else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::Quit),
        KeyCode::Char('q') | KeyCode::Esc => Some(Key::Quit),
        KeyCode::Up => Some(Key::ActionsUp),
        KeyCode::Down => Some(Key::ActionsDown),
        KeyCode::Home => Some(Key::ActionsFollow),
        KeyCode::PageUp => Some(Key::ErrorsUp),
        KeyCode::PageDown => Some(Key::ErrorsDown),
        _ => None,
    }
}

struct Totals {
    known_bytes: u64,
    done_bytes: u64,
    completed: usize,
}

fn totals(context: &RunContext) -> Totals {
    let mut totals = Totals {
        known_bytes: 0,
        done_bytes: 0,
        completed: 0,
    };
    for progress in context.progress {
        totals.done_bytes += progress.done_bytes();
        totals.known_bytes += progress.total().unwrap_or(0);
        if progress.state() == ActionState::Done {
            totals.completed += 1;
        }
    }
    totals
}

fn draw(frame: &mut Frame, context: &RunContext, dashboard: &mut Dashboard) {
    let summary = totals(context);
    dashboard.rate.sample(summary.done_bytes);

    let error_height = if dashboard.errors.is_empty() {
        0
    } else {
        (dashboard.errors.len().min(6) + 2) as u16
    };
    let [header_area, table_area, error_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(error_height),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    draw_header(frame, header_area, context, &summary, dashboard);
    draw_actions(frame, table_area, context, dashboard);
    if error_height > 0 {
        draw_errors(frame, error_area, dashboard);
    }
    draw_footer(frame, footer_area, &summary, dashboard);
}

fn draw_header(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    context: &RunContext,
    summary: &Totals,
    dashboard: &Dashboard,
) {
    let title = if dashboard.cancelling {
        " rclean — cancelling ".to_string()
    } else {
        format!(" rclean — {} ", display_path(context.root))
    };
    let line = Line::from(vec![
        Span::styled(
            format!("{}/{}", summary.completed, context.actions.len()),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" actions · "),
        Span::styled(
            human_size(summary.known_bytes),
            Style::new().fg(Color::Yellow),
        ),
        Span::raw(" found · "),
        Span::styled(
            human_size(summary.done_bytes),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" reclaimed · "),
        Span::raw(human_duration(context.started.elapsed())),
        Span::styled(
            "   (q quit · ↑↓ scroll)",
            Style::new().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).block(Block::bordered().title(title)), area);
}

fn draw_actions(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    context: &RunContext,
    dashboard: &mut Dashboard,
) {
    let visible = area.height.saturating_sub(3) as usize;
    if dashboard.follow_active {
        let first_unfinished = dashboard
            .finished
            .iter()
            .position(Option::is_none)
            .unwrap_or(0);
        dashboard.action_offset = first_unfinished.saturating_sub(visible.saturating_sub(1) / 2);
    }
    let max_offset = context.actions.len().saturating_sub(visible.max(1));
    dashboard.action_offset = dashboard.action_offset.min(max_offset);

    let rows = context.actions.iter().enumerate().map(|(index, action)| {
        let (status, style) = status_cell(&dashboard.finished[index], &context.progress[index]);
        Row::new(vec![
            Cell::from(action.kind.label()).style(Style::new().fg(Color::Magenta)),
            Cell::from(relative_display(&action.path, context.root)),
            Cell::from(status).style(style),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Fill(1),
            Constraint::Length(36),
        ],
    )
    .header(
        Row::new(vec!["KIND", "PATH", "PROGRESS"])
            .style(Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
    )
    .block(Block::bordered().title(" actions "));

    let mut state = TableState::new().with_offset(dashboard.action_offset);
    frame.render_stateful_widget(table, area, &mut state);
}

fn status_cell(finished: &Option<Finished>, progress: &Progress) -> (String, Style) {
    if let Some(finished) = finished {
        if finished.skipped {
            return ("skipped (build running)".to_string(), Style::new().fg(Color::Yellow));
        }
        let text = format!(
            "{:>9} in {}",
            human_size(finished.reclaimed_bytes),
            human_duration(finished.elapsed)
        );
        if finished.error_count > 0 {
            return (
                format!("{text}  ({} failed)", finished.error_count),
                Style::new().fg(Color::Red),
            );
        }
        return (text, Style::new().fg(Color::Green));
    }

    match progress.state() {
        ActionState::Pending => ("pending".to_string(), Style::new().fg(Color::DarkGray)),
        ActionState::Waiting => (
            "waiting for overlap".to_string(),
            Style::new().fg(Color::DarkGray),
        ),
        ActionState::Sizing => ("measuring…".to_string(), Style::new().fg(Color::Blue)),
        ActionState::Deleting | ActionState::Done => {
            let ratio = progress.ratio().unwrap_or(0.0);
            let total = progress.total().unwrap_or(0);
            (
                format!(
                    "{} {:>3.0}%  {} / {}",
                    text_bar(ratio),
                    ratio * 100.0,
                    human_size(progress.done_bytes()),
                    human_size(total)
                ),
                Style::new().fg(Color::Cyan),
            )
        }
    }
}

fn text_bar(ratio: f64) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * BAR_WIDTH as f64).round() as usize;
    format!(
        "{}{}",
        "▓".repeat(filled),
        "░".repeat(BAR_WIDTH.saturating_sub(filled))
    )
}

fn draw_errors(frame: &mut Frame, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let visible = area.height.saturating_sub(2) as usize;
    let max_offset = dashboard.errors.len().saturating_sub(visible.max(1));
    let offset = dashboard.error_offset.min(max_offset);
    let items: Vec<ListItem> = dashboard
        .errors
        .iter()
        .skip(offset)
        .take(visible)
        .map(|error| ListItem::new(Line::from(error.as_str())))
        .collect();

    frame.render_widget(
        List::new(items).style(Style::new().fg(Color::Red)).block(
            Block::bordered().title(format!(
                " problems ({}) — PgUp/PgDn ",
                dashboard.errors.len()
            )),
        ),
        area,
    );
}

fn draw_footer(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    summary: &Totals,
    dashboard: &Dashboard,
) {
    let ratio = if summary.known_bytes == 0 {
        0.0
    } else {
        (summary.done_bytes as f64 / summary.known_bytes as f64).clamp(0.0, 1.0)
    };
    let remaining = summary.known_bytes.saturating_sub(summary.done_bytes);
    let eta = if dashboard.rate.rate > 1.0 {
        human_duration(Duration::from_secs_f64(
            (remaining as f64 / dashboard.rate.rate).min(86_400.0),
        ))
    } else {
        "—".to_string()
    };

    frame.render_widget(
        Gauge::default()
            .block(Block::bordered().title(" overall "))
            .ratio(ratio)
            .gauge_style(Style::new().fg(Color::Green))
            .label(format!(
                "{:.0}%  ·  {}  ·  eta {eta}",
                ratio * 100.0,
                human_rate(dashboard.rate.rate)
            )),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // The original failure mode was a run that never ended, so the loop
    // terminating on its own is the property worth pinning down.
    #[test]
    fn the_run_loop_ends_once_every_action_reports() {
        use crate::action::{Action, ActionKind};
        use std::path::PathBuf;
        use std::sync::mpsc;

        let root = PathBuf::from(if cfg!(windows) { r"C:\dev" } else { "/dev" });
        let actions: Vec<Action> = (0..3)
            .map(|index| Action {
                kind: ActionKind::NodeModules,
                path: root.join(format!("p{index}")).join("node_modules"),
            })
            .collect();
        let progress: Vec<Arc<Progress>> = actions
            .iter()
            .map(|_| Arc::new(Progress::default()))
            .collect();

        let (sender, receiver) = mpsc::channel();
        for (index, action) in actions.iter().enumerate() {
            progress[index].set_state(ActionState::Done);
            sender
                .send((
                    index,
                    ActionReport {
                        kind: action.kind,
                        path: action.path.clone(),
                        reclaimed_bytes: 1024,
                        files_removed: 1,
                        elapsed: Duration::from_millis(1),
                        errors: Vec::new(),
                        error_count: 0,
                        skipped: false,
                    },
                ))
                .unwrap();
        }
        drop(sender);

        let cancel = AtomicBool::new(false);
        let outcome = run_plain(&RunContext {
            root: &root,
            actions: &actions,
            progress: &progress,
            receiver: &receiver,
            discovery_errors: &[],
            cancel: &cancel,
            started: Instant::now(),
        });

        assert_eq!(outcome.reports.len(), 3);
        assert!(outcome.already_logged);
        assert!(!outcome.cancelled);
    }

    // A worker dying without reporting must not wedge the loop.
    #[test]
    fn the_run_loop_ends_when_a_worker_never_reports() {
        use crate::action::{Action, ActionKind};
        use std::path::PathBuf;
        use std::sync::mpsc;

        let root = PathBuf::from(if cfg!(windows) { r"C:\dev" } else { "/dev" });
        let actions = vec![Action {
            kind: ActionKind::Scratch,
            path: root.join("scratch"),
        }];
        let progress = vec![Arc::new(Progress::default())];
        let (sender, receiver) = mpsc::channel();
        drop(sender); // the worker died before sending

        let cancel = AtomicBool::new(false);
        let outcome = run_plain(&RunContext {
            root: &root,
            actions: &actions,
            progress: &progress,
            receiver: &receiver,
            discovery_errors: &[],
            cancel: &cancel,
            started: Instant::now(),
        });

        assert!(outcome.reports.is_empty());
    }

    #[test]
    fn bar_endpoints_are_full_and_empty() {
        assert_eq!(text_bar(0.0), "░".repeat(BAR_WIDTH));
        assert_eq!(text_bar(1.0), "▓".repeat(BAR_WIDTH));
        assert_eq!(text_bar(2.0), "▓".repeat(BAR_WIDTH), "clamped");
        assert_eq!(text_bar(0.5).chars().count(), BAR_WIDTH);
    }

    #[test]
    fn finished_actions_report_size_and_failures() {
        let progress = Progress::default();
        let finished = Some(Finished {
            reclaimed_bytes: 2048,
            elapsed: Duration::from_secs(2),
            error_count: 3,
            skipped: false,
        });
        let (text, _) = status_cell(&finished, &progress);
        assert!(text.contains("2.0 KiB"), "{text}");
        assert!(text.contains("3 failed"), "{text}");
    }

    // Renders a full frame offscreen, since the dashboard itself only ever
    // runs against a real terminal.
    #[test]
    fn dashboard_renders_paths_progress_and_problems() {
        use crate::action::{Action, ActionKind};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::path::PathBuf;
        use std::sync::mpsc;

        let root = PathBuf::from(if cfg!(windows) { r"C:\dev" } else { "/dev" });
        let actions = vec![
            Action {
                kind: ActionKind::Cargo,
                path: root.join("web_template"),
            },
            Action {
                kind: ActionKind::NodeModules,
                path: root.join("ui").join("node_modules"),
            },
        ];
        let progress: Vec<Arc<Progress>> = actions
            .iter()
            .map(|_| Arc::new(Progress::default()))
            .collect();
        progress[0].set_state(ActionState::Deleting);
        progress[0].set_total(1000);
        progress[0].record_removed(420);
        progress[1].set_state(ActionState::Sizing);

        let (_sender, receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let discovery_errors = vec!["cannot read somewhere".to_string()];
        let context = RunContext {
            root: &root,
            actions: &actions,
            progress: &progress,
            receiver: &receiver,
            discovery_errors: &discovery_errors,
            cancel: &cancel,
            started: Instant::now(),
        };

        let mut dashboard = Dashboard::new(&context);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, &context, &mut dashboard))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        for expected in [
            "rclean",
            "web_template",
            "node_modules",
            "measuring",
            "42%",
            "problems (1)",
            "cannot read somewhere",
            "overall",
        ] {
            assert!(rendered.contains(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn skipped_actions_say_why() {
        let finished = Some(Finished {
            reclaimed_bytes: 0,
            elapsed: Duration::from_secs(0),
            error_count: 0,
            skipped: true,
        });
        let (text, _) = status_cell(&finished, &Progress::default());
        assert!(text.contains("build running"), "{text}");
    }
}
