//! Named service groups ("targets") you can switch between at runtime -
//! e.g. a `rescue` target with just a shell, and a `graphical` target
//! with the full desktop stack, instead of always running every
//! configured service at once. Every service belongs to exactly one
//! target (`target=`/`X-Target=`, defaulting to `DEFAULT_TARGET` - see
//! `config.rs`/`units.rs`); which target boots is `default_target=` in
//! `init.conf`; `mitosctl isolate <name>` (or the `ISOLATE` IPC command)
//! switches at runtime.
//!
//! Deliberately narrower than real systemd's targets: there's no
//! dependency graph *between* targets (a target can't `Wants=` another
//! target the way a unit can `Wants=` another unit - if `graphical`
//! needs `multi-user`'s services too, list them all under `graphical`),
//! and switching one reuses `Supervisor::reload_services`'s simpler
//! reconciliation rather than a fresh `spawn_all` - so, matching
//! `reload`'s own documented tradeoff (see `main.rs`), `after=`/
//! `requires=`/`after_ready=` ordering against services outside the
//! previous target isn't re-established on `isolate`. Good enough for
//! "which flat set of services is currently running", not a replacement
//! for systemd's transactional unit activation.

use crate::config::ServiceDef;

/// What boots when `init.conf` doesn't set `default_target=` and no
/// service sets `target=` - so a config written before this feature
/// existed still boots exactly the one flat list it always did.
pub const DEFAULT_TARGET: &str = "multi-user";

/// Every service definition whose `target` matches `name`, in the order
/// they appear in `all`.
pub fn services_in(all: &[ServiceDef], name: &str) -> Vec<ServiceDef> {
    all.iter().filter(|d| d.target == name).cloned().collect()
}

/// Every distinct target name mentioned across `all`, sorted - for
/// `mitosctl targets`.
pub fn list(all: &[ServiceDef]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for d in all {
        if !names.contains(&d.target) {
            names.push(d.target.clone());
        }
    }
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, target: &str) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            path: "/bin/true".into(),
            target: target.into(),
            ..Default::default()
        }
    }

    #[test]
    fn filters_by_target() {
        let all = vec![
            svc("a", "multi-user"),
            svc("b", "graphical"),
            svc("c", "multi-user"),
        ];
        let names: Vec<&str> = services_in(&all, "multi-user")
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn unknown_target_yields_no_services() {
        let all = vec![svc("a", "multi-user")];
        assert!(services_in(&all, "graphical").is_empty());
    }

    #[test]
    fn lists_distinct_targets_sorted() {
        let all = vec![
            svc("a", "graphical"),
            svc("b", "multi-user"),
            svc("c", "graphical"),
        ];
        assert_eq!(
            list(&all),
            vec!["graphical".to_string(), "multi-user".to_string()]
        );
    }
}
