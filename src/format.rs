use std::path::Path;
use std::time::Duration;

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
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

pub fn human_duration(duration: Duration) -> String {
    if duration.as_secs() >= 60 {
        format!("{:.1}m", duration.as_secs_f64() / 60.0)
    } else if duration.as_secs() >= 1 {
        format!("{:.2}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

pub fn human_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "—".to_string();
    }
    format!("{}/s", human_size(bytes_per_second as u64))
}

// Windows canonical paths carry a \\?\ verbatim prefix that is correct to use
// but noisy to read, so strip it for anything shown to a person.
#[cfg(windows)]
pub fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.into_owned()
    }
}

#[cfg(not(windows))]
pub fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn relative_display(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => display_path(path),
        Ok(relative) => display_path(relative),
        Err(_) => display_path(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_scale_to_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_stripped_for_display() {
        assert_eq!(display_path(Path::new(r"\\?\C:\dev\x")), r"C:\dev\x");
        assert_eq!(display_path(Path::new(r"\\?\UNC\srv\share")), r"\\srv\share");
        assert_eq!(display_path(Path::new(r"C:\dev\x")), r"C:\dev\x");
    }
}
