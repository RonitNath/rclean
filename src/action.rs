use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ActionKind {
    Cargo,
    Debug,
    NodeModules,
    PythonVenv,
    Scratch,
}

impl ActionKind {
    pub fn label(self) -> &'static str {
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
pub struct Action {
    pub kind: ActionKind,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct ActionReport {
    pub kind: ActionKind,
    pub path: PathBuf,
    pub reclaimed_bytes: u64,
    pub files_removed: u64,
    pub elapsed: Duration,
    pub errors: Vec<String>,
    /// Total failures including any elided beyond the reported sample.
    pub error_count: u64,
    /// Left alone on purpose (an active build holds the target), not a failure
    /// of the deletion itself.
    pub skipped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionState {
    Pending,
    Waiting,
    Sizing,
    Deleting,
    Done,
}

impl ActionState {
    fn encode(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::Waiting => 1,
            Self::Sizing => 2,
            Self::Deleting => 3,
            Self::Done => 4,
        }
    }

    fn decode(value: u8) -> Self {
        match value {
            1 => Self::Waiting,
            2 => Self::Sizing,
            3 => Self::Deleting,
            4 => Self::Done,
            _ => Self::Pending,
        }
    }
}

// Live per-action counters. Workers write, the UI thread polls at frame rate,
// which keeps the display off the completion channel entirely.
#[derive(Default)]
pub struct Progress {
    state: AtomicU8,
    total_bytes: AtomicU64,
    total_known: AtomicBool,
    done_bytes: AtomicU64,
    done_files: AtomicU64,
}

impl Progress {
    pub fn state(&self) -> ActionState {
        ActionState::decode(self.state.load(Ordering::Relaxed))
    }

    pub fn set_state(&self, state: ActionState) {
        self.state.store(state.encode(), Ordering::Relaxed);
    }

    pub fn set_total(&self, bytes: u64) {
        self.total_bytes.store(bytes, Ordering::Relaxed);
        self.total_known.store(true, Ordering::Release);
    }

    pub fn total(&self) -> Option<u64> {
        self.total_known
            .load(Ordering::Acquire)
            .then(|| self.total_bytes.load(Ordering::Relaxed))
    }

    pub fn done_bytes(&self) -> u64 {
        self.done_bytes.load(Ordering::Relaxed)
    }

    pub fn record_removed(&self, bytes: u64) {
        self.done_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.done_files.fetch_add(1, Ordering::Relaxed);
    }

    pub fn ratio(&self) -> Option<f64> {
        match self.total() {
            Some(0) => Some(1.0),
            Some(total) => Some((self.done_bytes() as f64 / total as f64).clamp(0.0, 1.0)),
            None => None,
        }
    }
}
