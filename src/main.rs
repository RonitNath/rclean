use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::channel;
use std::thread;
use threadpool::ThreadPool;

// Function to run cargo clean in a directory
fn run_cargo_clean(dir: PathBuf) {
    println!("Cleaning {:?}", dir);
    Command::new("cargo")
        .current_dir(dir)
        .arg("clean")
        .output()
        .expect("Failed to run cargo clean");
}

// Function to recursively find directories with Cargo.toml files
fn find_cargo_directories(dir: PathBuf, pool: &ThreadPool) {
    let entries = fs::read_dir(dir).expect("Failed to read directory");
    for entry in entries {
        if let Ok(entry) = entry {
            let path = entry.path();
            if path.is_dir() {
                if path.join("Cargo.toml").is_file() {
                    let dir_clone = path.clone();
                    pool.execute(move || {
                        run_cargo_clean(dir_clone);
                    });
                } else {
                    find_cargo_directories(path, pool);
                }
            }
        }
    }
}

fn main() {
    // Number of threads in the thread pool
    let num_threads = num_cpus::get();

    let pool = ThreadPool::new(num_threads);

    // Starting point: the current directory
    let current_dir = std::env::current_dir()
        .expect("Failed to get current directory")
        .parent()
        .expect("Has parent dir")
        .to_path_buf();
    find_cargo_directories(current_dir, &pool);

    // Shutdown the thread pool and wait for all threads to finish
    pool.join();

    println!("Cargo clean complete!");
}
