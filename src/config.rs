//! Tiny, dependency-free config file parser.
//!
//! We deliberately don't pull in `serde`/`toml` here — for a binary that
//! only ever parses one small file at boot, a hand-rolled parser is
//! smaller, has no build-time cost, and is one less thing that can break
//! before a real root filesystem is even mounted.

use crate::logging::Level;
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    pub path: String,
    pub args: Vec<String>,
    pub critical: bool,
    pub restart: RestartPolicy,
    /// Bytes, enforced via the service's cgroup (see `cgroups.rs`). `None`
    /// means no limit.
    pub memory_limit: Option<u64>,
    /// Service names this should start after (spawn-order only - see
    /// `supervisor::topological_order`).
    pub after: Vec<String>,
    /// Service names this should wait for `READY=1` from before starting,
    /// up to a bounded timeout (see `supervisor::wait_for_ready`). A
    /// stronger, opt-in guarantee than `after` - most services only need
    /// ordering, not a blocking wait.
    pub after_ready: Vec<String>,
    /// Username or bare uid to run as (see `users.rs`). `None` means run
    /// as whatever mitos-init itself runs as (root, as PID 1).
    pub user: Option<String>,
    /// Group name or bare gid to run as. `None` means the default group
    /// for `user` (or root's, if `user` is also unset).
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub hostname: Option<String>,
    pub loglevel: Level,
    pub shutdown_timeout_secs: u64,
    pub services: Vec<ServiceDef>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hostname: Some("mitos".to_string()),
            loglevel: Level::Info,
            shutdown_timeout_secs: 5,
            services: vec![ServiceDef {
                name: "shell".to_string(),
                path: "/bin/mitos-shell".to_string(),
                args: vec![],
                critical: true,
                restart: RestartPolicy::Never,
                memory_limit: None,
                after: vec![],
                after_ready: vec![],
                user: None,
                group: None,
            }],
        }
    }
}

/// Loads `/etc/mitos/init.conf` if present, otherwise falls back to a
/// single-service default (mitos-shell, falling back to /bin/sh at spawn
/// time) so the system is always bootable even with no config on disk.
pub fn load_or_default(path: &str) -> Config {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Config::default(),
    }
}

/// The service list used when `cmdline::rescue_requested` is true: just a
/// plain shell, bypassing whatever's in `init.conf`/`services.d/` -
/// config being broken is exactly the situation this needs to survive.
pub fn rescue_service() -> ServiceDef {
    ServiceDef {
        name: "rescue".to_string(),
        path: "/bin/sh".to_string(),
        args: vec![],
        critical: true,
        restart: RestartPolicy::Never,
        memory_limit: None,
        after: vec![],
        after_ready: vec![],
        user: None,
        group: None,
    }
}

fn parse(text: &str) -> Config {
    let mut cfg = Config {
        services: Vec::new(),
        ..Config::default()
    };

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("service ") {
            match parse_service(rest) {
                Ok(svc) => cfg.services.push(svc),
                Err(e) => eprintln!("mitos-init [WARN]: skipping bad service line ({e}): {line}"),
            }
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "hostname" => cfg.hostname = Some(value.to_string()),
                "loglevel" => {
                    if let Some(lvl) = Level::parse(value) {
                        cfg.loglevel = lvl;
                    }
                }
                "shutdown_timeout" => {
                    if let Ok(secs) = value.parse() {
                        cfg.shutdown_timeout_secs = secs;
                    }
                }
                _ => eprintln!("mitos-init [WARN]: unknown config key '{key}'"),
            }
        }
    }

    if cfg.services.is_empty() {
        cfg.services = Config::default().services;
    }
    cfg
}

/// Merges services from `init.conf`'s inline `service` lines with those
/// loaded from `/etc/mitos/services.d/*.service` unit files (see
/// `units.rs`). A name collision keeps whichever was seen first and warns
/// about the rest, rather than silently letting one replace the other.
pub fn merge_services(mut base: Vec<ServiceDef>, extra: Vec<ServiceDef>) -> Vec<ServiceDef> {
    for svc in extra {
        if base.iter().any(|s| s.name == svc.name) {
            eprintln!(
                "mitos-init [WARN]: duplicate service name '{}', keeping the first one seen",
                svc.name
            );
        } else {
            base.push(svc);
        }
    }
    base
}

fn parse_name_list(value: &str) -> Vec<String> {
    value.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn parse_service(rest: &str) -> std::result::Result<ServiceDef, String> {
    let mut parts = rest.split_whitespace();
    let name = parts.next().ok_or("missing service name")?.to_string();

    let mut path = None;
    let mut args = Vec::new();
    let mut critical = false;
    let mut restart = RestartPolicy::OnFailure;
    let mut memory_limit = None;
    let mut after = Vec::new();
    let mut after_ready = Vec::new();
    let mut user = None;
    let mut group = None;

    for field in parts {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| format!("bad field '{field}'"))?;
        let value = value.trim_matches('"');
        match key {
            "path" => path = Some(value.to_string()),
            "args" => args = parse_name_list(value),
            "critical" => critical = value.eq_ignore_ascii_case("true"),
            "restart" => {
                restart = match value {
                    "always" => RestartPolicy::Always,
                    "never" => RestartPolicy::Never,
                    _ => RestartPolicy::OnFailure,
                };
            }
            "mem_max" => memory_limit = crate::cgroups::parse_size(value),
            "after" => after = parse_name_list(value),
            "after_ready" => after_ready = parse_name_list(value),
            "user" => user = Some(value.to_string()),
            "group" => group = Some(value.to_string()),
            _ => {}
        }
    }

    let path = path.ok_or("missing path=")?;
    Ok(ServiceDef {
        name,
        path,
        args,
        critical,
        restart,
        memory_limit,
        after,
        after_ready,
        user,
        group,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_svc(name: &str, path: &str) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            path: path.into(),
            args: vec![],
            critical: false,
            restart: RestartPolicy::Never,
            memory_limit: None,
            after: vec![],
            after_ready: vec![],
            user: None,
            group: None,
        }
    }

    #[test]
    fn parses_global_settings() {
        let cfg = parse("hostname=myhost\nloglevel=debug\nshutdown_timeout=15\n");
        assert_eq!(cfg.hostname.as_deref(), Some("myhost"));
        assert_eq!(cfg.loglevel, Level::Debug);
        assert_eq!(cfg.shutdown_timeout_secs, 15);
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let cfg = parse("# a comment\n\nhostname=x\n");
        assert_eq!(cfg.hostname.as_deref(), Some("x"));
    }

    #[test]
    fn falls_back_to_default_service_when_none_declared() {
        let cfg = parse("hostname=x\n");
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "shell");
    }

    #[test]
    fn parses_a_service_line() {
        let cfg = parse(
            "service web path=/usr/bin/web args=--port,8080 critical=false restart=always mem_max=256M user=nobody group=nogroup after=db after_ready=cache\n",
        );
        assert_eq!(cfg.services.len(), 1);
        let svc = &cfg.services[0];
        assert_eq!(svc.name, "web");
        assert_eq!(svc.path, "/usr/bin/web");
        assert_eq!(svc.args, vec!["--port".to_string(), "8080".to_string()]);
        assert!(!svc.critical);
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert_eq!(svc.memory_limit, Some(256 * 1024 * 1024));
        assert_eq!(svc.user.as_deref(), Some("nobody"));
        assert_eq!(svc.group.as_deref(), Some("nogroup"));
        assert_eq!(svc.after, vec!["db".to_string()]);
        assert_eq!(svc.after_ready, vec!["cache".to_string()]);
    }

    #[test]
    fn rejects_a_service_missing_path() {
        assert!(parse_service("web critical=true").is_err());
    }

    #[test]
    fn merge_keeps_first_on_name_collision() {
        let base = vec![base_svc("shell", "/bin/a")];
        let extra = vec![base_svc("shell", "/bin/b")];
        let merged = merge_services(base, extra);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].path, "/bin/a");
    }
}
