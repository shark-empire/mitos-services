//! Tiny, dependency-free config file parser.
//!
//! We deliberately don't pull in `serde`/`toml` here — for a binary that
//! only ever parses one small file at boot, a hand-rolled parser is
//! smaller, has no build-time cost, and is one less thing that can break
//! before a real root filesystem is even mounted.

use crate::logging::Level;
use std::fs;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    #[default]
    Never,
}

#[derive(Debug, Clone, Default)]
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
    /// `supervisor::topological_order`). Also populated indirectly from
    /// other services' `before`/`requires`/`wants` - see
    /// `supervisor::effective_after`.
    pub after: Vec<String>,
    /// Service names this should wait for `READY=1` from before starting,
    /// up to a bounded timeout (see `supervisor::wait_for_ready`). A
    /// stronger, opt-in guarantee than `after` - most services only need
    /// ordering, not a blocking wait.
    pub after_ready: Vec<String>,
    /// Service names that should start after *this* one - the inverse of
    /// `after`, folded into the named services' effective `after` list
    /// at spawn time rather than tracked as its own ordering concept.
    pub before: Vec<String>,
    /// Hard dependencies: if a named service is configured but didn't
    /// end up running, this service is skipped too (logged, not fatal to
    /// boot). Also implies ordering, same as `after`.
    pub requires: Vec<String>,
    /// Soft dependencies: ordering only (same effect as adding these
    /// names to `after`) - doesn't block this service from starting if
    /// the named one fails or isn't configured.
    pub wants: Vec<String>,
    /// Username or bare uid to run as (see `users.rs`). `None` means run
    /// as whatever mitos-init itself runs as (root, as PID 1).
    pub user: Option<String>,
    /// Group name or bare gid to run as. `None` means the default group
    /// for `user` (or root's, if `user` is also unset).
    pub group: Option<String>,
    /// If set, this service is expected to send `WATCHDOG=1` at least
    /// this often (via `$NOTIFY_SOCKET`); missing that deadline is
    /// treated as a failure - see `supervisor::expired_watchdogs`. `None`
    /// means no watchdog is expected (most services).
    pub watchdog_timeout: Option<Duration>,
    /// Extra environment variables for this service, in addition to
    /// `NOTIFY_SOCKET` (always injected - see `notify.rs`). From
    /// `Environment=KEY=VAL` (unit files - repeatable, and
    /// space-separated for more than one on a line, matching real
    /// systemd) or `environment=KEY=VAL,KEY2=VAL2` (`init.conf` inline).
    pub environment: Vec<(String, String)>,
    /// Directory to `chdir()` into before exec -
    /// `WorkingDirectory=`/`workdir=`. `None` inherits mitos-services'
    /// own cwd.
    pub working_dir: Option<String>,
    /// Which target (see `targets.rs`) this service belongs to -
    /// `X-Target=` (unit files) / `target=` (`init.conf` inline).
    /// Left empty by `Default`/test code, same as every other field here
    /// - real configured services always get a concrete value (defaulted
    ///   to `targets::DEFAULT_TARGET`) from `parse_service`/`parse_unit`,
    ///   never from this struct's own `Default` impl.
    pub target: String,
    /// `PrivateTmp=`/`private_tmp=` - a private tmpfs `/tmp` for this
    /// service. See `sandbox::ServiceSandbox`.
    pub private_tmp: bool,
    /// `ProtectSystem=`/`protect_system=` - `/usr`, `/boot`, and `/etc`
    /// made read-only for this service. See `sandbox::ServiceSandbox`.
    pub protect_system: bool,
    /// `NoNewPrivileges=`/`no_new_privileges=` - this service (and
    /// anything it execs) can never gain privileges it doesn't already
    /// have, even via a setuid or file-capability binary. See
    /// `sandbox::ServiceSandbox`.
    pub no_new_privileges: bool,
    /// `OOMScoreAdjust=`/`oom_score_adjust=` - adjusts how likely the
    /// kernel's OOM killer is to pick this service first (-1000 to
    /// 1000, more negative is more protected). `None` leaves the
    /// kernel's default alone. See `oom.rs`.
    pub oom_score_adjust: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub hostname: Option<String>,
    pub loglevel: Level,
    pub shutdown_timeout_secs: u64,
    /// Which target (see `targets.rs`) boots by default - `default_target=`.
    /// Services not in this target aren't started at boot; switch with
    /// `mitosctl isolate <name>`.
    pub default_target: String,
    pub services: Vec<ServiceDef>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hostname: Some("mitos".to_string()),
            loglevel: Level::Info,
            shutdown_timeout_secs: 5,
            default_target: crate::targets::DEFAULT_TARGET.to_string(),
            services: vec![ServiceDef {
                name: "shell".to_string(),
                path: "/bin/mitos-shell".to_string(),
                critical: true,
                target: crate::targets::DEFAULT_TARGET.to_string(),
                ..Default::default()
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
                "default_target" => cfg.default_target = value.to_string(),
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
    value
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
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
    let mut before = Vec::new();
    let mut requires = Vec::new();
    let mut wants = Vec::new();
    let mut user = None;
    let mut group = None;
    let mut watchdog_timeout = None;
    let mut environment = Vec::new();
    let mut working_dir = None;
    let mut target = None;
    let mut private_tmp = false;
    let mut protect_system = false;
    let mut no_new_privileges = false;
    let mut oom_score_adjust = None;

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
            "before" => before = parse_name_list(value),
            "requires" => requires = parse_name_list(value),
            "wants" => wants = parse_name_list(value),
            "user" => user = Some(value.to_string()),
            "group" => group = Some(value.to_string()),
            "watchdog_sec" => watchdog_timeout = value.parse().ok().map(Duration::from_secs),
            "environment" => environment = parse_env_list(value),
            "workdir" => working_dir = Some(value.to_string()),
            "target" => target = Some(value.to_string()),
            "private_tmp" => private_tmp = value.eq_ignore_ascii_case("true"),
            "protect_system" => protect_system = value.eq_ignore_ascii_case("true"),
            "no_new_privileges" => no_new_privileges = value.eq_ignore_ascii_case("true"),
            "oom_score_adjust" => oom_score_adjust = value.parse().ok(),
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
        before,
        requires,
        wants,
        user,
        group,
        watchdog_timeout,
        environment,
        working_dir,
        target: target.unwrap_or_else(|| crate::targets::DEFAULT_TARGET.to_string()),
        private_tmp,
        protect_system,
        no_new_privileges,
        oom_score_adjust,
    })
}

/// Parses `environment=`'s comma-separated `KEY=VALUE` pairs - the same
/// list convention `after=`/`args=` already use. A value containing a
/// literal comma isn't representable this way; use a `.service` unit
/// file's `Environment=` (space-separated, not comma) instead if that's
/// needed.
fn parse_env_list(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            pair.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_svc(name: &str, path: &str) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            path: path.into(),
            ..Default::default()
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
    fn parses_before_requires_wants_and_watchdog() {
        let cfg = parse(
            "service web path=/usr/bin/web before=proxy requires=db wants=cache watchdog_sec=30\n",
        );
        let svc = &cfg.services[0];
        assert_eq!(svc.before, vec!["proxy".to_string()]);
        assert_eq!(svc.requires, vec!["db".to_string()]);
        assert_eq!(svc.wants, vec!["cache".to_string()]);
        assert_eq!(svc.watchdog_timeout, Some(Duration::from_secs(30)));
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

    #[test]
    fn parses_environment_workdir_and_target() {
        let cfg = parse(
            "service web path=/usr/bin/web environment=RUST_LOG=info,PORT=8080 workdir=/var/lib/web target=graphical\n",
        );
        let svc = &cfg.services[0];
        assert_eq!(
            svc.environment,
            vec![
                ("RUST_LOG".to_string(), "info".to_string()),
                ("PORT".to_string(), "8080".to_string())
            ]
        );
        assert_eq!(svc.working_dir.as_deref(), Some("/var/lib/web"));
        assert_eq!(svc.target, "graphical");
    }

    #[test]
    fn defaults_target_to_multi_user_when_unspecified() {
        let cfg = parse("service web path=/usr/bin/web\n");
        assert_eq!(cfg.services[0].target, crate::targets::DEFAULT_TARGET);
    }

    #[test]
    fn parses_default_target() {
        let cfg = parse("default_target=graphical\n");
        assert_eq!(cfg.default_target, "graphical");
    }

    #[test]
    fn parses_sandboxing_fields() {
        let cfg = parse(
            "service web path=/usr/bin/web private_tmp=true protect_system=true no_new_privileges=true\n",
        );
        let svc = &cfg.services[0];
        assert!(svc.private_tmp);
        assert!(svc.protect_system);
        assert!(svc.no_new_privileges);
    }

    #[test]
    fn parses_oom_score_adjust() {
        let cfg = parse("service web path=/usr/bin/web oom_score_adjust=-500\n");
        assert_eq!(cfg.services[0].oom_score_adjust, Some(-500));
    }
}
