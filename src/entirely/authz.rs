//! Authorization for entirely-mode holds.
//!
//! `Hold` grants a root-only capability (the persistent `SleepDisabled`
//! power setting, i.e. `pmset disablesleep`), so the helper checks who is
//! asking: root, administrators, and members of a dedicated grant group are
//! allowed. The check uses only the peer's kernel-supplied uid; group
//! membership is resolved helper-side via `getgrouplist`, which follows
//! directory-services (including nested) membership and takes effect on the
//! user's next request — no re-login needed. Every lookup failure denies
//! (fail closed).
//!
//! SECURITY INVARIANT: the caffeinate2 binaries must NOT be installed
//! setuid/setgid. This check trusts the peer's *effective* uid (the kernel's
//! `LOCAL_PEERCRED`) as the caller's identity. A setuid/setgid client would
//! present an effective id that does not match the human actually running it,
//! which would let it escalate (or self-deny) here. The helper itself runs as
//! a root LaunchDaemon and the CLI/tray run with the invoking user's real
//! credentials; keep them that way.

use std::ffi::{CStr, CString};

/// Dedicated group an administrator can add standard accounts to in order to
/// allow entirely mode (`--install-helper` creates it).
pub const GRANT_GROUP: &str = "caffeinate2";
const ADMIN_GROUP: &str = "admin";

struct User {
    name: String,
    primary_gid: libc::gid_t,
}

fn user_for_uid(uid: libc::uid_t) -> Option<User> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let ret = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut result,
        )
    };
    if ret != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(pwd.pw_name) }
        .to_str()
        .ok()?
        .to_string();
    if name.is_empty() {
        return None;
    }
    Some(User {
        name,
        primary_gid: pwd.pw_gid,
    })
}

fn gid_for_group(name: &str) -> Option<libc::gid_t> {
    let cname = CString::new(name).ok()?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::group = std::ptr::null_mut();
    let ret = unsafe {
        libc::getgrnam_r(
            cname.as_ptr(),
            &raw mut grp,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut result,
        )
    };
    if ret != 0 || result.is_null() {
        return None;
    }
    Some(grp.gr_gid)
}

/// Upper bound on the group-list buffer we'll grow to. Real accounts have far
/// fewer groups than this; the cap only stops an unbounded loop on a
/// pathological directory while still being generous enough to never deny a
/// legitimate user.
const MAX_GROUP_CAPACITY: libc::c_int = 65_536;

fn group_ids_for_user(user: &User) -> Option<Vec<libc::gid_t>> {
    let cname = CString::new(user.name.as_str()).ok()?;
    let mut capacity: libc::c_int = 32;
    // macOS getgrouplist returns -1 when the array is too small (with
    // *ngroups set to how many fit), so grow and retry until it succeeds or we
    // hit the sane upper bound (rather than failing closed at a small cap).
    loop {
        let mut groups = vec![0_i32; capacity.cast_unsigned() as usize];
        let mut count = capacity;
        let ret = unsafe {
            libc::getgrouplist(
                cname.as_ptr(),
                user.primary_gid.cast_signed(),
                groups.as_mut_ptr(),
                &raw mut count,
            )
        };
        if ret != -1 {
            groups.truncate(count.max(0).cast_unsigned() as usize);
            return Some(groups.into_iter().map(i32::cast_unsigned).collect());
        }
        if capacity >= MAX_GROUP_CAPACITY {
            tracing::warn!(
                "user '{}' belongs to more than {MAX_GROUP_CAPACITY} groups; \
                 denying entirely-mode authorization (fail closed)",
                user.name
            );
            return None;
        }
        capacity = capacity.saturating_mul(2).min(MAX_GROUP_CAPACITY);
    }
}

/// Whether `uid` may take an entirely-mode hold. Fails closed: any failure
/// to resolve the user or their groups denies.
#[must_use]
pub fn uid_may_hold(uid: libc::uid_t) -> bool {
    if uid == 0 {
        return true;
    }
    let Some(user) = user_for_uid(uid) else {
        return false;
    };
    let Some(groups) = group_ids_for_user(&user) else {
        return false;
    };
    [ADMIN_GROUP, GRANT_GROUP]
        .iter()
        .any(|name| gid_for_group(name).is_some_and(|gid| groups.contains(&gid)))
}

/// Denial message sent to the client; starts with the
/// [`crate::entirely::helper_ipc::is_authorization_error`] prefix and includes the
/// exact grant command for this user.
#[must_use]
pub fn denial_message(uid: libc::uid_t) -> String {
    // Single-quote the resolved name so the suggested command is copy-paste-safe
    // even for accounts with unusual characters; a bare name could otherwise be
    // re-split or interpreted by the shell.
    let who = user_for_uid(uid).map_or_else(|| format!("uid {uid}"), |user| sh_single_quote(&user.name));
    format!(
        "not authorized: entirely mode requires an administrator account or membership in the \
         '{GRANT_GROUP}' group; an administrator can grant it with: \
         sudo dseditgroup -o edit -a {who} -t user {GRANT_GROUP}"
    )
}

/// Wrap `s` in single quotes for safe inclusion in a shell command shown to the
/// user, escaping embedded single quotes.
fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_always_allowed() {
        assert!(uid_may_hold(0));
    }

    #[test]
    fn current_user_resolves() {
        let uid = nix::unistd::getuid().as_raw();
        let user = user_for_uid(uid).expect("current user should resolve");
        assert!(!user.name.is_empty());
        let groups = group_ids_for_user(&user).expect("groups should resolve");
        assert!(groups.contains(&user.primary_gid));
    }

    #[test]
    fn nonexistent_uid_is_denied() {
        // Fail closed: an unresolvable uid must not be authorized.
        assert!(!uid_may_hold(u32::MAX - 7));
    }

    #[test]
    fn denial_message_has_prefix_and_grant_command() {
        let message = denial_message(u32::MAX - 7);
        assert!(message.starts_with("not authorized"));
        assert!(message.contains("dseditgroup"));
        assert!(message.contains(GRANT_GROUP));
    }
}
