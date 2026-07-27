//! Canonical credential scopes (plan §2, decision A-5).
//!
//! Seven tokens, frozen. Six grantable plus one reserved:
//!
//! ```text
//! recall:v2  memory:read  memory:write  memory:export  memory:history  admin
//! proxy:chat   (reserved — see `Scope::is_reserved`)
//! ```
//!
//! Two properties are load-bearing:
//!
//! * **No scope implies another.** In particular `admin` grants no
//!   memory-content capability. An admin credential may delete an agent or run
//!   an archival sweep, but it cannot read, export, list, diff or inspect the
//!   history of memory content without also holding the matching content
//!   scope. Collapsing the two recreates the "one key opens everything"
//!   property the plan exists to remove.
//! * **Reserved scopes are never satisfied by [`ScopeSet::allows`].**
//!   `proxy:chat` is storable and round-trips, but Path B (decision A-3) is
//!   disabled and its enablement is not part of this step.
//!
//! This module *defines and validates* scopes. It does not apply them to any
//! endpoint — that is plan §10 step 5.

use std::collections::BTreeSet;
use std::fmt;

/// A single canonical capability token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    RecallV2,
    MemoryRead,
    MemoryWrite,
    MemoryExport,
    MemoryHistory,
    Admin,
    ProxyChat,
}

/// Every canonical scope, in wire order. Kept as an array rather than derived
/// so that adding a variant without deciding its wire token fails to compile.
pub const CANONICAL_SCOPES: [Scope; 7] = [
    Scope::RecallV2,
    Scope::MemoryRead,
    Scope::MemoryWrite,
    Scope::MemoryExport,
    Scope::MemoryHistory,
    Scope::Admin,
    Scope::ProxyChat,
];

impl Scope {
    /// Wire form, shared by the `credentials.scopes` column and the
    /// `credentials_scopes_ck` CHECK in migration 0029.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecallV2 => "recall:v2",
            Self::MemoryRead => "memory:read",
            Self::MemoryWrite => "memory:write",
            Self::MemoryExport => "memory:export",
            Self::MemoryHistory => "memory:history",
            Self::Admin => "admin",
            Self::ProxyChat => "proxy:chat",
        }
    }

    /// Reserved scopes are canonical — they parse, store and round-trip — but
    /// [`ScopeSet::allows`] never returns `true` for one.
    ///
    /// `proxy:chat` gates Path B (`/v1/chat/completions`), which decision A-3
    /// keeps disabled by default. Enabling it needs an operator opt-in that
    /// does not exist yet, so until then holding the scope must confer nothing.
    pub const fn is_reserved(self) -> bool {
        matches!(self, Self::ProxyChat)
    }

    /// Exact-match parse against the canonical set.
    ///
    /// The distinction between [`ScopeError::Unknown`] and
    /// [`ScopeError::Malformed`] is for the operator's benefit: "you invented a
    /// scope" and "you typed one wrong" have different fixes. Neither is
    /// accepted.
    pub fn parse(raw: &str) -> Result<Self, ScopeError> {
        if raw.is_empty() {
            return Err(ScopeError::Malformed {
                raw: raw.to_string(),
                why: "empty",
            });
        }
        if raw.trim() != raw {
            return Err(ScopeError::Malformed {
                raw: raw.to_string(),
                why: "surrounding whitespace",
            });
        }
        if raw.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(ScopeError::Malformed {
                raw: raw.to_string(),
                why: "scopes are lowercase",
            });
        }
        if !is_well_formed(raw) {
            return Err(ScopeError::Malformed {
                raw: raw.to_string(),
                why: "expected `<resource>` or `<resource>:<action>` of [a-z0-9]",
            });
        }

        CANONICAL_SCOPES
            .iter()
            .copied()
            .find(|s| s.as_str() == raw)
            .ok_or_else(|| ScopeError::Unknown(raw.to_string()))
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `<resource>` or `<resource>:<action>`, each segment non-empty `[a-z0-9]`.
///
/// Hand-rolled rather than a regex: this is the only pattern in the crate that
/// would need one, and a dependency for seven string literals is not a trade
/// worth making.
fn is_well_formed(raw: &str) -> bool {
    let mut segments = raw.split(':');
    let Some(first) = segments.next() else {
        return false;
    };
    let second = segments.next();
    if segments.next().is_some() {
        return false; // more than one ':'
    }
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    };
    ok(first) && second.map(ok).unwrap_or(true)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("unknown scope {0:?}; the canonical set is fixed by plan decision A-5")]
    Unknown(String),
    #[error("scope {0:?} appears more than once")]
    Duplicate(String),
    #[error("malformed scope {raw:?}: {why}")]
    Malformed { raw: String, why: &'static str },
}

/// A validated, deduplicated, ordered set of scopes.
///
/// `Default` is the empty set, which grants nothing — the fail-closed value, so
/// unlike [`super::AeonAuthContext`] there is no hazard in deriving it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeSet(BTreeSet<Scope>);

impl ScopeSet {
    /// Parse a stored or operator-supplied scope list.
    ///
    /// Rejects unknown, duplicate and malformed tokens. Duplicates are an error
    /// rather than silently collapsed: a list containing the same scope twice
    /// means whoever produced it was not tracking what it already granted, and
    /// quietly accepting it hides that.
    pub fn parse_list<S: AsRef<str>>(raw: &[S]) -> Result<Self, ScopeError> {
        let mut set = BTreeSet::new();
        for token in raw {
            let token = token.as_ref();
            let scope = Scope::parse(token)?;
            if !set.insert(scope) {
                return Err(ScopeError::Duplicate(token.to_string()));
            }
        }
        Ok(Self(set))
    }

    /// Whether this set satisfies `scope`.
    ///
    /// Returns `false` for every reserved scope regardless of membership — see
    /// [`Scope::is_reserved`]. Use [`ScopeSet::holds_reserved`] to ask the
    /// different question "was this reserved scope granted", which Path B must
    /// pair with its own operator opt-in.
    ///
    /// There is no implication graph. `allows(MemoryExport)` is false for an
    /// `admin`-only credential (decision A-5).
    pub fn allows(&self, scope: Scope) -> bool {
        !scope.is_reserved() && self.0.contains(&scope)
    }

    /// Membership test for reserved scopes, deliberately separate from
    /// [`ScopeSet::allows`].
    ///
    /// Kept distinct so that a future author enabling Path B has to write the
    /// enablement check explicitly rather than deleting a special case from
    /// `allows` and silently activating a capability decision A-3 gates.
    pub fn holds_reserved(&self, scope: Scope) -> bool {
        scope.is_reserved() && self.0.contains(&scope)
    }

    /// Wire form for persistence. Ordered by [`Scope`]'s `Ord`, so a round-trip
    /// is stable and two equal sets serialise identically.
    pub fn to_wire(&self) -> Vec<String> {
        self.0.iter().map(|s| s.as_str().to_string()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Scope> + '_ {
        self.0.iter().copied()
    }
}

impl fmt::Display for ScopeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for scope in self.iter() {
            if !first {
                f.write_str(",")?;
            }
            f.write_str(scope.as_str())?;
            first = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(tokens: &[&str]) -> ScopeSet {
        ScopeSet::parse_list(tokens).expect("tokens should parse")
    }

    #[test]
    fn every_canonical_scope_round_trips() {
        for scope in CANONICAL_SCOPES {
            assert_eq!(Scope::parse(scope.as_str()), Ok(scope));
        }
    }

    #[test]
    fn the_canonical_set_is_exactly_the_frozen_seven() {
        // Guards against a scope being added without a plan amendment: A-5
        // froze six grantable scopes plus the reserved proxy:chat.
        let tokens: Vec<&str> = CANONICAL_SCOPES.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            tokens,
            vec![
                "recall:v2",
                "memory:read",
                "memory:write",
                "memory:export",
                "memory:history",
                "admin",
                "proxy:chat",
            ]
        );
    }

    #[test]
    fn unknown_scopes_are_rejected() {
        assert_eq!(
            Scope::parse("memory:delete"),
            Err(ScopeError::Unknown("memory:delete".into()))
        );
        // Well-formed but invented.
        assert!(matches!(Scope::parse("root"), Err(ScopeError::Unknown(_))));
    }

    #[test]
    fn malformed_scopes_are_rejected_and_distinguished_from_unknown() {
        for (raw, why) in [
            ("", "empty"),
            (" admin", "surrounding whitespace"),
            ("admin ", "surrounding whitespace"),
            ("ADMIN", "scopes are lowercase"),
            ("Memory:Read", "scopes are lowercase"),
        ] {
            match Scope::parse(raw) {
                Err(ScopeError::Malformed { why: got, .. }) => assert_eq!(got, why, "for {raw:?}"),
                other => panic!("{raw:?} should be malformed, got {other:?}"),
            }
        }
        for raw in [
            "memory:",
            ":read",
            "memory::read",
            "a:b:c",
            "memory read",
            "mem_ory",
        ] {
            assert!(
                matches!(Scope::parse(raw), Err(ScopeError::Malformed { .. })),
                "{raw:?} should be malformed"
            );
        }
    }

    #[test]
    fn duplicate_scopes_are_rejected_not_collapsed() {
        assert_eq!(
            ScopeSet::parse_list(&["memory:read", "admin", "memory:read"]),
            Err(ScopeError::Duplicate("memory:read".into()))
        );
    }

    #[test]
    fn a_list_containing_one_bad_token_rejects_the_whole_list() {
        // Partial acceptance would silently narrow (or widen) a credential.
        assert!(ScopeSet::parse_list(&["memory:read", "memory:delete"]).is_err());
    }

    // ── Decision A-5: admin implies nothing ──────────────────────────────────

    #[test]
    fn admin_does_not_imply_memory_export() {
        let admin = set(&["admin"]);
        assert!(admin.allows(Scope::Admin));
        assert!(!admin.allows(Scope::MemoryExport));
    }

    #[test]
    fn admin_implies_no_memory_content_scope_at_all() {
        let admin = set(&["admin"]);
        for scope in [
            Scope::RecallV2,
            Scope::MemoryRead,
            Scope::MemoryWrite,
            Scope::MemoryExport,
            Scope::MemoryHistory,
        ] {
            assert!(!admin.allows(scope), "admin must not imply {scope}");
        }
    }

    #[test]
    fn no_scope_implies_any_other() {
        // The full matrix, not just the admin row: any implication at all would
        // be a widening nobody asked for.
        for granted in CANONICAL_SCOPES {
            let only = ScopeSet::parse_list(&[granted.as_str()]).unwrap();
            for probed in CANONICAL_SCOPES {
                let expected = probed == granted && !probed.is_reserved();
                assert_eq!(
                    only.allows(probed),
                    expected,
                    "granting {granted} must {} {probed}",
                    if expected { "allow" } else { "not allow" }
                );
            }
        }
    }

    // ── Reserved scope ───────────────────────────────────────────────────────

    #[test]
    fn proxy_chat_is_never_allowed_even_when_granted() {
        let granted = set(&["proxy:chat", "admin"]);
        assert!(!granted.allows(Scope::ProxyChat));
        // …but it is recorded, so Path B can find it once A-3 enables it.
        assert!(granted.holds_reserved(Scope::ProxyChat));
    }

    #[test]
    fn holds_reserved_answers_only_for_reserved_scopes() {
        let granted = set(&["memory:read"]);
        assert!(!granted.holds_reserved(Scope::MemoryRead));
        assert!(granted.allows(Scope::MemoryRead));
    }

    // ── Round-trip ───────────────────────────────────────────────────────────

    #[test]
    fn to_wire_is_ordered_and_reparses() {
        let original = set(&["admin", "memory:read", "recall:v2"]);
        let wire = original.to_wire();
        assert_eq!(wire, vec!["recall:v2", "memory:read", "admin"]);
        assert_eq!(ScopeSet::parse_list(&wire).unwrap(), original);
    }

    #[test]
    fn an_empty_set_grants_nothing() {
        let empty = ScopeSet::default();
        assert!(empty.is_empty());
        for scope in CANONICAL_SCOPES {
            assert!(!empty.allows(scope));
        }
    }

    #[test]
    fn every_canonical_token_satisfies_the_migration_check_constraint() {
        // The CHECK in 0029 lists the tokens literally. If these drift, a
        // credential the application considers valid becomes unwritable.
        let in_migration = include_str!("../../migrations/0029_credentials.sql");
        for scope in CANONICAL_SCOPES {
            assert!(
                in_migration.contains(&format!("'{}'", scope.as_str())),
                "migration 0029 does not list {scope}"
            );
        }
    }
}
