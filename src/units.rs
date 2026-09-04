//! Loads declarative per-service files from a drop-in directory:
//! `/etc/mitos/services.d/*.service`.
//!
//! Two different ideas borrowed from two different systems, on purpose:
//! - **Organization**, from launchd: one small file per service, dropped
//!   in a directory and picked up automatically - no central registry to
//!   hand-edit. (Real launchd uses XML property lists for the file syntax;
//!   we don't - see below.)
//! - **Syntax**, from systemd: plain `[Section]` / `Key=Value` unit files.
//!   Parsing real XML plists correctly, with their handful of typed value
//!   encodings, would pull in an XML/plist stack for a binary that
//!   otherwise has zero parsing dependencies by design (see `config.rs`).
//!   systemd's unit format needs none of that and is at least as familiar
//!   to the Linux side of "work like Linux and macOS".
//!
//! Supported keys (a deliberately small, well-understood subset - anything
//! else is ignored rather than rejected, so a real systemd unit file with
//! more sections/keys than we act on still loads):
//!   `[Service]`  `ExecStart=`, `Restart=(no|always|on-failure)`, `User=`,
//!                `Group=`, `After=`/`Before=`/`Requires=`/`Wants=`
//!                (comma-separated name lists - real systemd repeats the
//!                key instead, but this reuses the same list syntax
//!                `args=`/`after=` already use in `init.conf`; semantics
//!                are also intentionally simplified from real systemd's -
//!                see `supervisor::effective_after`/`spawn_all` - since
//!                this project supervises a fixed configured list rather
//!                than real systemd's transactional unit activation),
//!                `WatchdogSec=` (seconds - see
//!                `supervisor::expired_watchdogs`), `X-Critical=(true|false)`,
//!                `X-AfterReady=name,name` (`X-`-prefixed since it isn't
//!                real systemd syntax - see `supervisor::wait_for_ready`)
//! The service name comes from the filename (`mitos-shell.service` ->
//! `mitos-shell`), matching systemd's own convention.

use crate::config::{RestartPolicy, ServiceDef};
use crate::logging;
use std::fs;
use std::path::Path;
use std::time::Duration;

const SERVICES_DIR: &str = "/etc/mitos/services.d";

/// Loads every `*.service` file in `SERVICES_DIR`, in sorted filename
/// order. A missing directory isn't an error - not every install uses
/// drop-in units, some just use init.conf's inline `service` lines.
pub fn load_all() -> Vec<ServiceDef> {
    let Ok(entries) = fs::read_dir(SERVICES_DIR) else {
        return Vec::new();
    };

    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "service").unwrap_or(false))
        .collect();
    paths.sort();

    let mut services = Vec::new();
    for path in paths {
        match fs::read_to_string(&path) {
            Ok(text) => match parse_unit(&path, &text) {
                Ok(svc) => services.push(svc),
                Err(e) => logging::warn(&format!("skipping {}: {e}", path.display())),
            },
            Err(e) => logging::warn(&format!("couldn't read {}: {e}", path.display())),
        }
    }
    services
}

fn parse_name_list(value: &str) -> Vec<String> {
    value.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn parse_unit(path: &Path, text: &str) -> Result<ServiceDef, String> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("unreadable filename")?
        .to_string();

    let mut section = String::new();
    let mut exec_start: Option<String> = None;
    let mut restart = RestartPolicy::Never; // systemd's own default is "no"
    let mut critical = false;
    let mut memory_limit = None;
    let mut after = Vec::new();
    let mut after_ready = Vec::new();
    let mut before = Vec::new();
    let mut requires = Vec::new();
    let mut wants = Vec::new();
    let mut user = None;
    let mut group = None;
    let mut watchdog_timeout = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());

        if section != "Service" {
            continue; // [Unit]/[Install]/etc: parsed for forward-compat, acted on for none
        }

        match key {
            "ExecStart" => exec_start = Some(value.to_string()),
            "Restart" => {
                restart = match value {
                    "always" => RestartPolicy::Always,
                    "no" => RestartPolicy::Never,
                    "on-failure" => RestartPolicy::OnFailure,
                    other => {
                        logging::warn(&format!(
                            "{}: unrecognized Restart={other}, treating as on-failure",
                            path.display()
                        ));
                        RestartPolicy::OnFailure
                    }
                };
            }
            "X-Critical" => critical = value.eq_ignore_ascii_case("true"),
            "MemoryMax" => memory_limit = crate::cgroups::parse_size(value),
            "After" => after = parse_name_list(value),
            "X-AfterReady" => after_ready = parse_name_list(value),
            "Before" => before = parse_name_list(value),
            "Requires" => requires = parse_name_list(value),
            "Wants" => wants = parse_name_list(value),
            "User" => user = Some(value.to_string()),
            "Group" => group = Some(value.to_string()),
            "WatchdogSec" => watchdog_timeout = value.parse().ok().map(Duration::from_secs),
            _ => {} // unrecognized [Service] key: ignored, not rejected
        }
    }

    let exec_start = exec_start.ok_or("missing ExecStart= in [Service]")?;
    // Simple whitespace splitting, unlike systemd's own ExecStart quoting
    // rules - fine for the plain "binary plus flags" case this covers.
    let mut parts = exec_start.split_whitespace();
    let bin = parts.next().ok_or("empty ExecStart=")?.to_string();
    let args = parts.map(String::from).collect();

    Ok(ServiceDef {
        name,
        path: bin,
        args,
        critical,
        restart,
        memory_limit,
        after,
        after_ready,
        before,
        requires,
        wants,
        user,
        group,
        watchdog_timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_unit() {
        let text = "[Service]\nExecStart=/bin/mitos-shell\n";
        let svc = parse_unit(Path::new("mitos-shell.service"), text).unwrap();
        assert_eq!(svc.name, "mitos-shell");
        assert_eq!(svc.path, "/bin/mitos-shell");
        assert!(svc.args.is_empty());
        assert_eq!(svc.restart, RestartPolicy::Never);
        assert!(!svc.critical);
    }

    #[test]
    fn parses_args_restart_critical_and_memory_max() {
        let text = "[Unit]\nDescription=test\n\n[Service]\nExecStart=/usr/bin/foo --flag bar\nRestart=always\nX-Critical=true\nMemoryMax=128M\n";
        let svc = parse_unit(Path::new("foo.service"), text).unwrap();
        assert_eq!(svc.path, "/usr/bin/foo");
        assert_eq!(svc.args, vec!["--flag".to_string(), "bar".to_string()]);
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert!(svc.critical);
        assert_eq!(svc.memory_limit, Some(128 * 1024 * 1024));
    }

    #[test]
    fn parses_dependencies_and_identity() {
        let text = "[Service]\nExecStart=/usr/bin/web\nAfter=db,cache\nX-AfterReady=db\nUser=nobody\nGroup=nogroup\n";
        let svc = parse_unit(Path::new("web.service"), text).unwrap();
        assert_eq!(svc.after, vec!["db".to_string(), "cache".to_string()]);
        assert_eq!(svc.after_ready, vec!["db".to_string()]);
        assert_eq!(svc.user.as_deref(), Some("nobody"));
        assert_eq!(svc.group.as_deref(), Some("nogroup"));
    }

    #[test]
    fn parses_before_requires_wants_and_watchdog() {
        let text = "[Service]\nExecStart=/usr/bin/proxy\nBefore=web\nRequires=db\nWants=cache\nWatchdogSec=15\n";
        let svc = parse_unit(Path::new("proxy.service"), text).unwrap();
        assert_eq!(svc.before, vec!["web".to_string()]);
        assert_eq!(svc.requires, vec!["db".to_string()]);
        assert_eq!(svc.wants, vec!["cache".to_string()]);
        assert_eq!(svc.watchdog_timeout, Some(Duration::from_secs(15)));
    }

    #[test]
    fn ignores_keys_outside_the_service_section() {
        let text = "[Unit]\nExecStart=/should/be/ignored\n\n[Service]\nExecStart=/real/path\n";
        let svc = parse_unit(Path::new("x.service"), text).unwrap();
        assert_eq!(svc.path, "/real/path");
    }

    #[test]
    fn rejects_a_unit_with_no_execstart() {
        let text = "[Service]\nRestart=always\n";
        assert!(parse_unit(Path::new("x.service"), text).is_err());
    }
}
