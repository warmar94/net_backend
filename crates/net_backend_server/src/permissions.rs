//! Permissions: named rights finer than roles. A module (or the game) declares the permissions it
//! checks ([`Module::permissions`](crate::Module::permissions),
//! [`NetBackendServer::permission`](crate::NetBackendServer::permission)), each with the roles that
//! hold it by default; the operator grants permissions to roles in `[permissions]`; code asks
//! "may this caller do X" with [`AuthContext::has_permission`] or the [`RequirePermission`]
//! extractor.
//!
//! **The rules:**
//! - A permission is a dotted name: `lobbies.manage`, `game.mute` (lower-case letters, digits and
//!   `_` in each part, at least two parts, at most 64 bytes, [`is_valid_permission`]). A module's
//!   permissions start with its name and a dot.
//! - A caller has a permission when one of its roles holds it. The `admin` role holds every
//!   declared permission.
//! - `[permissions]` maps a role to its permissions and **replaces** the defaults for that role;
//!   roles not listed keep the declared defaults. A name in `[permissions]` that nothing declares
//!   stops the build (a typo never silently grants nothing), and so does listing `admin`.
//! - A permission nobody declared is never held (also not by `admin`): declare what you check.
//! - Roles are read per request (the accounts module's roles), so a granted role works on the
//!   caller's next request; permissions themselves come from code and the configuration file.
//!
//! ```toml
//! [permissions]
//! moderator = ["lobbies.manage", "game.mute"]   # moderators: exactly these two
//! support = ["game.mute"]
//! ```
//!
//! ```
//! use net_backend_server::permissions::{Permission, PermissionName, RequirePermission};
//! use net_backend_server::{AppError, AuthContext, AppState, Config, NetBackendServer};
//!
//! // The game's own permission: held by `admin` and by `moderator` unless `[permissions]` says otherwise.
//! const MUTE: Permission = Permission::new("game.mute", "Mute a player in the game's own chat").granted_to(&["moderator"]);
//!
//! struct Mute;
//! impl PermissionName for Mute {
//!     const NAME: &'static str = MUTE.name();
//! }
//!
//! // An extractor: 401 without a caller, 403 `forbidden` without the permission.
//! async fn mute(RequirePermission(staff, ..): RequirePermission<Mute>) -> String {
//!     format!("muted by {}", staff.user_id)
//! }
//!
//! // The same check where the permission depends on the request.
//! fn may_mute(state: &AppState, caller: &AuthContext) -> Result<(), AppError> {
//!     caller.require_permission(state, MUTE.name())
//! }
//!
//! let server = NetBackendServer::new(Config::default()).permission(MUTE);
//! # let _ = (server, mute, may_mute);
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use axum::extract::FromRequestParts;
use http::request::Parts;

use crate::auth::AuthContext;
use crate::error::{AppError, Error};
use crate::state::AppState;

/// The longest permission name, in bytes.
pub const MAX_PERMISSION_BYTES: usize = 64;

/// Whether `name` is a valid permission name: at most [`MAX_PERMISSION_BYTES`] bytes, at least two
/// parts separated by `.`, each part 1 or more of `a-z`, `0-9` and `_`, starting with a letter
/// (`lobbies.manage`, `game.chat.mute`).
pub fn is_valid_permission(name: &str) -> bool {
    name.len() <= MAX_PERMISSION_BYTES
        && name.split('.').count() >= 2
        && name.split('.').all(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_lowercase) && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// A declared permission: its name, what it allows (for people: the documentation of the
/// server), and the roles that hold it unless `[permissions]` lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permission {
    name: &'static str,
    description: &'static str,
    roles: &'static [&'static str],
}

impl Permission {
    /// A permission held by `admin` only (until `[permissions]` grants it to other roles).
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name, description, roles: &[] }
    }

    /// The same permission, held by these roles by default too.
    pub const fn granted_to(mut self, roles: &'static [&'static str]) -> Self {
        self.roles = roles;
        self
    }

    /// The name (`lobbies.manage`).
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// What it allows.
    pub const fn description(&self) -> &'static str {
        self.description
    }

    /// The roles that hold it by default (`admin` always does).
    pub const fn default_roles(&self) -> &'static [&'static str] {
        self.roles
    }
}

/// A permission name as a type, for [`RequirePermission`].
pub trait PermissionName: Send + Sync + 'static {
    /// The permission (`lobbies.manage`).
    const NAME: &'static str;
}

/// Every declared permission and which roles hold it: a state value (`state.get::<Permissions>()`,
/// `Ext<Permissions>`) of every built server. Built from the modules' and the game's declarations
/// and `[permissions]`.
#[derive(Clone, Debug, Default)]
pub struct Permissions {
    declared: BTreeMap<&'static str, Permission>,
    by_role: BTreeMap<String, BTreeSet<&'static str>>,
}

impl Permissions {
    /// The permissions of `declared` (with the module declaring each, `None` for the game) and the
    /// `[permissions]` grants; every problem at once.
    pub(crate) fn build(declared: Vec<(Option<&str>, Permission)>, grants: &BTreeMap<String, Vec<String>>) -> Result<Self, Error> {
        let mut problems = Vec::new();
        let mut map: BTreeMap<&'static str, Permission> = BTreeMap::new();
        let mut by_role: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        for (module, permission) in declared {
            let name = permission.name;
            let owner = module.map_or_else(|| "the game".to_string(), |m| format!("module `{m}`"));
            if !is_valid_permission(name) {
                problems.push(format!("{owner}: `{name}` is not a valid permission name (dotted parts of a-z 0-9 _, at most {MAX_PERMISSION_BYTES} bytes)"));
                continue;
            }
            if let Some(module) = module {
                if !name.starts_with(&format!("{module}.")) {
                    problems.push(format!("module `{module}`: the permission `{name}` must start with `{module}.`"));
                }
            }
            if map.insert(name, permission).is_some() {
                problems.push(format!("the permission `{name}` is declared twice"));
                continue;
            }
            for role in permission.roles {
                if !net_backend_protocol::admin::is_valid_role(role) {
                    problems.push(format!("{owner}: the permission `{name}` names `{role}`, which is not a valid role name"));
                }
                by_role.entry((*role).to_string()).or_default().insert(name);
            }
        }
        for (role, names) in grants {
            if role == net_backend_protocol::admin::ADMIN_ROLE {
                problems.push("permissions.admin: the admin role holds every permission; leave it out".to_string());
                continue;
            }
            if !net_backend_protocol::admin::is_valid_role(role) {
                problems.push(format!("permissions: `{role}` is not a valid role name"));
                continue;
            }
            let mut set = BTreeSet::new();
            for name in names {
                match map.get_key_value(name.as_str()) {
                    Some((key, _)) => {
                        set.insert(*key);
                    }
                    None => problems.push(format!("permissions.{role}: no module or the game declares the permission `{name}` (a typo?)")),
                }
            }
            by_role.insert(role.clone(), set);
        }
        if problems.is_empty() {
            Ok(Self { declared: map, by_role })
        } else {
            Err(Error::Config(problems))
        }
    }

    /// Whether `caller` holds `permission`: one of its roles holds it, or it has the `admin` role
    /// and the permission is declared.
    pub fn allows(&self, caller: &AuthContext, permission: &str) -> bool {
        if !self.declared.contains_key(permission) {
            tracing::debug!(permission, "a permission nobody declared was checked: refused");
            return false;
        }
        caller.roles.iter().any(|role| role == net_backend_protocol::admin::ADMIN_ROLE || self.by_role.get(role).is_some_and(|set| set.contains(permission)))
    }

    /// `Ok` if `caller` holds `permission`, else 403 `forbidden`.
    pub fn require(&self, caller: &AuthContext, permission: &str) -> Result<(), AppError> {
        if self.allows(caller, permission) {
            Ok(())
        } else {
            Err(AppError::forbidden(format!("this needs the permission `{permission}`")))
        }
    }

    /// Every declared permission, by name.
    pub fn declared(&self) -> impl Iterator<Item = &Permission> {
        self.declared.values()
    }

    /// The permissions these roles hold, by name (`admin`: every declared one).
    pub fn of_roles(&self, roles: &[String]) -> Vec<&'static str> {
        if roles.iter().any(|r| r == net_backend_protocol::admin::ADMIN_ROLE) {
            return self.declared.keys().copied().collect();
        }
        let mut out = BTreeSet::new();
        for role in roles {
            if let Some(set) = self.by_role.get(role) {
                out.extend(set.iter().copied());
            }
        }
        out.into_iter().collect()
    }

    /// The roles that hold `permission` (besides `admin`), by name.
    pub fn roles_with(&self, permission: &str) -> Vec<&str> {
        self.by_role.iter().filter(|(_, set)| set.contains(permission)).map(|(role, _)| role.as_str()).collect()
    }
}

impl AuthContext {
    /// Whether the caller holds `permission` (see [`crate::permissions`]).
    pub fn has_permission(&self, state: &AppState, permission: &str) -> bool {
        state.get::<Permissions>().is_some_and(|p| p.allows(self, permission))
    }

    /// `Ok` if the caller holds `permission`, else 403 `forbidden` (for permissions chosen at run
    /// time; the [`RequirePermission`] extractor covers fixed ones).
    pub fn require_permission(&self, state: &AppState, permission: &str) -> Result<(), AppError> {
        match state.get::<Permissions>() {
            Some(permissions) => permissions.require(self, permission),
            None => Err(AppError::forbidden(format!("this needs the permission `{permission}`"))),
        }
    }
}

/// An extractor: the caller's [`AuthContext`], only if it holds the permission `P` (401 without a
/// caller, 403 `forbidden` without the permission).
pub struct RequirePermission<P: PermissionName>(pub AuthContext, pub PhantomData<P>);

impl<P: PermissionName> std::fmt::Debug for RequirePermission<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RequirePermission").field(&P::NAME).field(&self.0).finish()
    }
}

impl<P: PermissionName> FromRequestParts<AppState> for RequirePermission<P> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let context = <AuthContext as FromRequestParts<AppState>>::from_request_parts(parts, state).await?;
        context.require_permission(state, P::NAME)?;
        Ok(RequirePermission(context, PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use net_backend_protocol::UserId;

    const MANAGE: Permission = Permission::new("lobbies.manage", "Manage every lobby").granted_to(&["moderator"]);
    const MUTE: Permission = Permission::new("game.mute", "Mute a player");

    fn caller(roles: &[&str]) -> AuthContext {
        AuthContext::new(UserId(1)).with_roles(roles.iter().map(|r| r.to_string()).collect())
    }

    #[test]
    fn names() {
        for good in ["lobbies.manage", "game.chat.mute", "a.b", "x1.y_2"] {
            assert!(is_valid_permission(good), "{good}");
        }
        for bad in ["", "lobbies", "Lobbies.manage", "a..b", ".a", "a.", "1a.b", "a.B", "a-b.c", &format!("a.{}", "b".repeat(63))] {
            assert!(!is_valid_permission(bad), "{bad}");
        }
    }

    #[test]
    fn defaults_grants_and_admin() {
        let none = BTreeMap::new();
        let permissions = Permissions::build(vec![(Some("lobbies"), MANAGE), (None, MUTE)], &none).expect("build");
        assert!(permissions.allows(&caller(&["moderator"]), "lobbies.manage"));
        assert!(!permissions.allows(&caller(&["moderator"]), "game.mute"), "MUTE: admin only by default");
        assert!(permissions.allows(&caller(&["admin"]), "game.mute") && permissions.allows(&caller(&["admin"]), "lobbies.manage"));
        assert!(!permissions.allows(&caller(&[]), "lobbies.manage"));
        assert!(!permissions.allows(&caller(&["admin"]), "game.undeclared"), "undeclared: never held");
        assert_eq!(permissions.require(&caller(&[]), "game.mute").err().map(|e| e.status().as_u16()), Some(403));
        assert_eq!(permissions.of_roles(&["admin".into()]), ["game.mute", "lobbies.manage"]);
        assert_eq!(permissions.roles_with("lobbies.manage"), ["moderator"]);
        assert_eq!(permissions.declared().count(), 2);

        // `[permissions]` replaces a listed role's defaults and grants to new roles.
        let mut grants = BTreeMap::new();
        grants.insert("moderator".to_string(), vec!["game.mute".to_string()]);
        grants.insert("support".to_string(), vec!["game.mute".to_string(), "lobbies.manage".to_string()]);
        let permissions = Permissions::build(vec![(Some("lobbies"), MANAGE), (None, MUTE)], &grants).expect("build");
        assert!(!permissions.allows(&caller(&["moderator"]), "lobbies.manage"), "replaced");
        assert!(permissions.allows(&caller(&["moderator"]), "game.mute"));
        assert_eq!(permissions.of_roles(&["support".into(), "moderator".into()]), ["game.mute", "lobbies.manage"]);
    }

    #[test]
    fn problems() {
        let mut grants = BTreeMap::new();
        grants.insert("admin".to_string(), vec![]);
        grants.insert("Mods".to_string(), vec![]);
        grants.insert("support".to_string(), vec!["game.mutte".to_string()]);
        let declared = vec![
            (Some("chat"), MANAGE),
            (None, MUTE),
            (None, MUTE),
            (None, Permission::new("bad", "x")),
            (None, Permission::new("game.kick", "x").granted_to(&["Bad Role"])),
        ];
        let error = Permissions::build(declared, &grants).err().map(|e| e.to_string()).unwrap_or_default();
        for part in [
            "must start with `chat.`",
            "`game.mute` is declared twice",
            "`bad` is not a valid permission",
            "`Bad Role`",
            "permissions.admin",
            "`Mods`",
            "`game.mutte`",
        ] {
            assert!(error.contains(part), "{part}: {error}");
        }
    }
}
