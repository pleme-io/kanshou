//! Canonical socket path resolution. Same algorithm consumed by the
//! server (binds) and the client (discovers).

use std::path::PathBuf;

/// Directory holding every kanshou socket on this host.
///
/// - macOS: `$HOME/Library/Application Support/kanshou`
/// - linux: `$XDG_RUNTIME_DIR/kanshou` if set, else `/tmp/kanshou-<uid>`
///
/// The directory is created on demand; existing dir + perms preserved.
/// Accept a directory override only when it is ABSOLUTE.
///
/// Every arm below used to take its environment variable on trust. A relative
/// or empty value then made the whole socket directory relative to the
/// process's cwd, which for a DISCOVERY mechanism is the worst possible
/// failure: a GUI started from `$HOME` and an MCP server started from a repo
/// each bind a different directory, each sees zero peers, and both report
/// themselves perfectly healthy. Nothing errors — they simply never meet.
///
/// `$HOME` unset made this reachable on macOS too, not just via a hostile
/// variable: `var_os("HOME").unwrap_or_default()` yields an empty path, and
/// pushing onto it gives the relative `Library/Application Support/kanshou`.
fn absolute(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let p = PathBuf::from(value?);
    if p.is_absolute() && !p.as_os_str().is_empty() {
        Some(p)
    } else {
        None
    }
}

#[must_use]
pub fn socket_dir() -> PathBuf {
    // Test/CI hermeticity seam: a process that must never discover
    // (or be discovered by) the operator's LIVE instances points this
    // at a private dir. Without it, a test suite running while the
    // real GUI is open forwards queries to the operator's session —
    // the mado mcp_config_get flake class (2026-06-11).
    if let Some(dir) = absolute(std::env::var_os("KANSHOU_SOCKET_DIR")) {
        return dir;
    }
    if cfg!(target_os = "macos") {
        if let Some(mut p) = absolute(std::env::var_os("HOME")) {
            p.push("Library/Application Support/kanshou");
            return p;
        }
    } else if let Ok(base) = okiba::Okiba::for_app("kanshou").base(okiba::Tier::Runtime) {
        // okiba applies the same absolute-only rule to $XDG_RUNTIME_DIR, and
        // returns NoSpecDefault rather than inventing a home-relative
        // fallback — the replacement below is kanshou's to choose.
        return base.join("kanshou");
    }
    // Per-UID, to avoid socket squatting on a shared /tmp. Absolute by
    // construction, so it is also the safe landing spot for every arm above
    // that declined a non-absolute value.
    let uid = unsafe { libc_geteuid() }.unwrap_or(0);
    PathBuf::from(format!("/tmp/kanshou-{uid}"))
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
// clippy's `unnecessary_wraps` is CFG-BLIND here: it lints against the
// `#[cfg(unix)]` arm alone, where the value is indeed always `Some`. The
// `#[cfg(not(unix))]` arm below returns `None`, so the Option is the whole
// point — unwrapping it would compile on this machine and break Windows.
#[allow(
    clippy::unnecessary_wraps,
    reason = "None on the non-unix arm; clippy only sees the cfg(unix) body"
)]
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

    /// The rule, in isolation: only an absolute value is accepted. Everything
    /// else is DECLINED so the caller falls through to a known-absolute
    /// replacement, rather than silently resolved against the cwd.
    #[test]
    fn only_absolute_overrides_are_accepted() {
        for bad in ["", ".", "relative/run", "run/user/1000"] {
            assert_eq!(
                absolute(Some(bad.into())),
                None,
                "{bad:?} must be declined, not joined"
            );
        }
        assert_eq!(
            absolute(Some("/run/user/1000".into())),
            Some(PathBuf::from("/run/user/1000"))
        );
        assert_eq!(absolute(None), None);
    }

    /// THE invariant, whatever this machine's environment happens to say. A
    /// relative socket directory means two kanshou peers started from
    /// different working directories never discover each other, while both
    /// report themselves healthy — so the absoluteness is the load-bearing
    /// property, not the particular directory chosen.
    #[test]
    fn the_socket_dir_is_always_absolute() {
        assert!(socket_dir().is_absolute(), "{:?}", socket_dir());
        assert!(socket_path("mado", 1234).is_absolute());
    }

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
