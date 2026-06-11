//! Canonical socket path resolution. Same algorithm consumed by the
//! server (binds) and the client (discovers).

use std::path::PathBuf;

/// Directory holding every kanshou socket on this host.
///
/// - macOS: `$HOME/Library/Application Support/kanshou`
/// - linux: `$XDG_RUNTIME_DIR/kanshou` if set, else `/tmp/kanshou-<uid>`
///
/// The directory is created on demand; existing dir + perms preserved.
#[must_use]
pub fn socket_dir() -> PathBuf {
    // Test/CI hermeticity seam: a process that must never discover
    // (or be discovered by) the operator's LIVE instances points this
    // at a private dir. Without it, a test suite running while the
    // real GUI is open forwards queries to the operator's session —
    // the mado mcp_config_get flake class (2026-06-11).
    if let Some(dir) = std::env::var_os("KANSHOU_SOCKET_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").unwrap_or_default();
        let mut p = PathBuf::from(home);
        p.push("Library/Application Support/kanshou");
        p
    } else {
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            let mut p = PathBuf::from(xdg);
            p.push("kanshou");
            return p;
        }
        // Fallback per-UID to avoid socket squatting on shared /tmp.
        let uid =
            unsafe { libc_geteuid() }.unwrap_or(0);
        PathBuf::from(format!("/tmp/kanshou-{uid}"))
    }
}

/// Canonical socket path for an app+pid pair.
#[must_use]
pub fn socket_path(app_name: &str, pid: u32) -> PathBuf {
    let mut p = socket_dir();
    p.push(format!("{app_name}-{pid}.sock"));
    p
}

/// Parse an app-name + PID out of a socket filename. Returns `None`
/// when the shape isn't `<name>-<pid>.sock`.
#[must_use]
pub fn parse_socket_name(name: &str) -> Option<(String, u32)> {
    let stem = name.strip_suffix(".sock")?;
    let dash = stem.rfind('-')?;
    let (app, pid_str) = stem.split_at(dash);
    let pid: u32 = pid_str.trim_start_matches('-').parse().ok()?;
    Some((app.to_string(), pid))
}

/// Best-effort `geteuid()` without pulling the `libc` crate. We only
/// need it on Linux's `/tmp` fallback path; macOS uses `$HOME` and
/// never reaches here. Returns `None` on Windows / unknown.
unsafe fn libc_geteuid() -> Option<u32> {
    #[cfg(unix)]
    {
        // `getuid` is signal-safe and always succeeds.
        // We deliberately don't depend on the libc crate — direct
        // FFI keeps the dep tree minimal for a substrate primitive.
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        Some(unsafe { getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_dir_env_override_wins() {
        unsafe { std::env::set_var("KANSHOU_SOCKET_DIR", "/tmp/kanshou-test-override") };
        assert_eq!(
            socket_dir(),
            std::path::PathBuf::from("/tmp/kanshou-test-override"),
            "KANSHOU_SOCKET_DIR must take precedence for hermetic tests"
        );
        unsafe { std::env::remove_var("KANSHOU_SOCKET_DIR") };
    }

    #[test]
    fn socket_path_format() {
        let p = socket_path("mado", 12345);
        assert!(p.to_string_lossy().ends_with("mado-12345.sock"));
    }

    #[test]
    fn parse_basic() {
        assert_eq!(
            parse_socket_name("mado-12345.sock"),
            Some(("mado".into(), 12345))
        );
    }

    #[test]
    fn parse_dashed_app() {
        // App name itself may contain dashes — the LAST dash is the
        // PID separator. `blackmatter-cli-99.sock` → `blackmatter-cli`, 99.
        assert_eq!(
            parse_socket_name("blackmatter-cli-99.sock"),
            Some(("blackmatter-cli".into(), 99))
        );
    }

    #[test]
    fn parse_rejects_non_sock() {
        assert_eq!(parse_socket_name("mado-12345.log"), None);
    }

    #[test]
    fn parse_rejects_no_pid() {
        assert_eq!(parse_socket_name("mado.sock"), None);
    }

    #[test]
    fn parse_rejects_bad_pid() {
        assert_eq!(parse_socket_name("mado-abc.sock"), None);
    }
}
