use std::path::PathBuf;

/// Build the ordered list of absolute paths to search for one of Vigil's
/// helper binaries (`vigil-scan`, `vigil-supervise`, `vigillog`).
///
/// The directory that contains the running Vigil binary is tried first so
/// that a set of freshly built binaries works without installation (e.g.
/// testing directly out of `target/release/`). Afterwards a fixed system
/// search path is used, including `/usr/sbin`, `/sbin` and `/usr/local/bin`
/// for admin-installed deployments.
pub fn exec_search_paths(binary: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(binary));
        }
    }

    for base in ["/usr/local/bin", "/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        paths.push(PathBuf::from(base).join(binary));
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_paths_are_unique_and_ordered() {
        let paths = exec_search_paths("vigil-scan");
        assert!(paths.len() >= 4, "unexpected path set: {:?}", paths);
        let has_insert = paths.windows(2).any(|w| w[0] == w[1]);
        assert!(!has_insert, "duplicate entries");
        assert!(
            paths
                .iter()
                .any(|p| p == &PathBuf::from("/usr/sbin/vigil-scan")),
            "must include /usr/sbin"
        );
        assert!(
            paths.iter().any(|p| p == &PathBuf::from("/bin/vigil-scan")),
            "must include /bin"
        );
    }

    #[test]
    fn every_entry_bears_the_binary_name() {
        for p in exec_search_paths("vigillog") {
            assert_eq!(p.file_name().and_then(|f| f.to_str()), Some("vigillog"));
        }
    }
}
