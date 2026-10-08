//! Filesystem conventions shared by osmic writers.

/// Prefix for temporary files that osmic creates next to an output while
/// writing it (they are renamed over the destination when complete).
///
/// The process id is included so a signal handler can remove exactly the
/// files of the process being interrupted.
pub fn temp_file_prefix() -> String {
    format!(".osmic-{}-", std::process::id())
}

/// Whether `file_name` is a temporary file created by process `pid`.
pub fn is_temp_file_of(file_name: &str, pid: u32) -> bool {
    file_name.starts_with(&format!(".osmic-{pid}-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_identifies_this_process() {
        let name = format!("{}abc.pmtiles.tmp", temp_file_prefix());
        assert!(is_temp_file_of(&name, std::process::id()));
        assert!(!is_temp_file_of(&name, std::process::id().wrapping_add(1)));
    }
}
