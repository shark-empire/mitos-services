//! Minimal `/etc/passwd` and `/etc/group` lookups, for resolving
//! `user=`/`group=` (`init.conf`) and `User=`/`Group=` (unit files)
//! service directives to the numeric ids `Command::uid`/`gid` need.
//!
//! Just enough to resolve a name or a bare numeric id - not a full NSS
//! implementation (no LDAP, no systemd-homed, ...). Real systemd falls
//! back to exactly this (`/etc/passwd`, `/etc/group`) too when nothing
//! fancier is configured, so this covers the common case correctly even
//! though it isn't the whole picture.

use std::fs;

pub fn resolve_uid(spec: &str) -> Option<u32> {
    if let Ok(n) = spec.parse::<u32>() {
        return Some(n);
    }
    let text = fs::read_to_string("/etc/passwd").ok()?;
    resolve_from_text(&text, spec, 2)
}

pub fn resolve_gid(spec: &str) -> Option<u32> {
    if let Ok(n) = spec.parse::<u32>() {
        return Some(n);
    }
    let text = fs::read_to_string("/etc/group").ok()?;
    resolve_from_text(&text, spec, 2)
}

/// Both `/etc/passwd` and `/etc/group` are colon-separated, with the name
/// in field 0 and the numeric id in `id_field` (2, for both files).
fn resolve_from_text(text: &str, name: &str, id_field: usize) -> Option<u32> {
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first() == Some(&name) {
            return fields.get(id_field)?.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/:/usr/sbin/nologin\n";

    #[test]
    fn resolves_a_known_name() {
        assert_eq!(resolve_from_text(PASSWD, "nobody", 2), Some(65534));
    }

    #[test]
    fn returns_none_for_unknown_name() {
        assert_eq!(resolve_from_text(PASSWD, "ghost", 2), None);
    }

    #[test]
    fn numeric_specs_skip_the_lookup_entirely() {
        assert_eq!(resolve_uid("1000"), Some(1000));
        assert_eq!(resolve_gid("1000"), Some(1000));
    }
}
