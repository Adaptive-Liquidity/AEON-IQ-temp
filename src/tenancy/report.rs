//! Report model and the two renderings of it.
//!
//! One report object, two surfaces: a serde/JSON form for Step 4B's tooling and
//! a text form for whoever has to read it. Both are rendered from the *same*
//! value, so they cannot disagree about what was found.
//!
//! ## What must never appear here
//!
//! The audit reads tables holding memory content, retrieval queries, provider
//! output, credential MACs and embeddings. None of that belongs in a
//! diagnostic report, so the report model has no field that could carry it:
//! findings carry counts, reason codes, and a *safe identifier* that is either
//! a UUID/numeric surrogate key or a domain-separated SHA-256 pseudonym of a
//! caller-controlled text key. There is no free-text field derived
//! from row values anywhere in this file, which is what makes the
//! confidentiality tests a check on a structural property rather than on a
//! filter that could be forgotten.

use std::fmt::Write as _;

use serde::Serialize;

use super::audit::{Finding, ReasonCode, Severity};
use super::inventory::{
    self, ExcludedObject, MigrationPlan, OwnershipPath, RowIdentity, SessionSemantics, TableClass,
    Tranche,
};

/// A registry entry as it appears in a report or artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassifiedTable {
    pub table: &'static str,
    pub class: TableClass,
    /// How this table's example row identifier is built. Declared, never
    /// derived from the catalog.
    pub row_identity: RowIdentity,
    /// What a session reference means here, and the evidence for it.
    pub session_semantics: Option<SessionSemantics>,
    pub canonical_path: Option<OwnershipPath>,
    pub secondary_paths: &'static [OwnershipPath],
    pub consistency: Option<&'static str>,
    pub tranche: Tranche,
    pub plan: MigrationPlan,
    pub global_scope_evidence: Option<&'static str>,
    pub rationale: &'static str,
}

/// Whether one migration wave may proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrancheReadiness {
    pub tranche: Tranche,
    pub tables: Vec<String>,
    pub ready: bool,
    /// `table: REASON_CODE`, sorted. Empty exactly when `ready`.
    pub blocking_reasons: Vec<String>,
}

/// The whole result of one audit run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TenancyAuditReport {
    /// Bumped whenever this shape changes, so a consumer can refuse a report it
    /// does not understand rather than misread one.
    pub schema_version: &'static str,
    /// Injected, never read from the clock: it is the only field that would
    /// otherwise differ between two runs over one snapshot, and the
    /// determinism tests compare everything else byte for byte.
    pub generated_at: Option<String>,
    /// Changes whenever the registry changes, so a consumer can notice that the
    /// tenancy decisions moved under it.
    pub inventory_digest: String,
    pub discovered_application_tables: Vec<String>,
    pub excluded_objects: Vec<ExcludedObject>,
    pub classified_tables: Vec<ClassifiedTable>,
    pub blocking_count: i64,
    pub advisory_count: i64,
    pub findings: Vec<Finding>,
    pub tranche_readiness: Vec<TrancheReadiness>,
}

impl TenancyAuditReport {
    /// The verdict. Any blocking finding blocks — advisory findings are never
    /// permission to proceed, which is why this reads `blocking_count` and
    /// nothing else.
    pub fn is_blocked(&self) -> bool {
        self.blocking_count > 0
    }

    pub fn verdict(&self) -> &'static str {
        if self.is_blocked() {
            "BLOCKED"
        } else {
            "READY"
        }
    }

    /// The machine form. Sorted throughout: `findings` are sorted by the audit,
    /// `classified_tables` follow the registry's own ordering, and every
    /// collection is an ordered `Vec` rather than a `HashMap`, so serialization
    /// is stable without a post-processing pass.
    /// Consumed by the Step 4A tests and by Step 4B's migration tooling; the
    /// binary has no caller while the diagnostic surface stays unwired.
    #[allow(dead_code)]
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// The operator form.
impl std::fmt::Display for TenancyAuditReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "# AEON tenancy audit — Step 4A")?;
        writeln!(f)?;
        writeln!(f, "verdict            {}", self.verdict())?;
        writeln!(f, "schema_version     {}", self.schema_version)?;
        writeln!(f, "inventory_digest   {}", self.inventory_digest)?;
        if let Some(at) = &self.generated_at {
            writeln!(f, "generated_at       {at}")?;
        }
        writeln!(
            f,
            "tables             {} discovered, {} classified, {} excluded",
            self.discovered_application_tables.len(),
            self.classified_tables.len(),
            self.excluded_objects.len()
        )?;
        writeln!(
            f,
            "findings           {} blocking, {} advisory",
            self.blocking_count, self.advisory_count
        )?;

        writeln!(f)?;
        writeln!(f, "## Excluded objects")?;
        writeln!(f)?;
        if self.excluded_objects.is_empty() {
            writeln!(f, "(none)")?;
        } else {
            for object in &self.excluded_objects {
                writeln!(f, "  {:<28} {}", object.name, object.reason)?;
            }
        }

        writeln!(f)?;
        writeln!(f, "## Findings")?;
        writeln!(f)?;
        if self.findings.is_empty() {
            writeln!(f, "(none)")?;
        } else {
            for finding in &self.findings {
                writeln!(
                    f,
                    "  [{}] {} {} ({} row(s))",
                    finding.severity, finding.reason_code, finding.table_name, finding.count
                )?;
                writeln!(f, "        {}", finding.diagnostic)?;
                if let Some(path) = &finding.ownership_path {
                    writeln!(f, "        path: {path}")?;
                }
                if let Some(id) = &finding.row_identifier {
                    writeln!(f, "        example: {id}")?;
                }
            }
        }

        writeln!(f)?;
        writeln!(f, "## Tranche readiness")?;
        writeln!(f)?;
        for tranche in &self.tranche_readiness {
            writeln!(
                f,
                "  {:<45} {}",
                tranche.tranche.as_str(),
                if tranche.ready { "READY" } else { "BLOCKED" }
            )?;
            for reason in &tranche.blocking_reasons {
                writeln!(f, "        {reason}")?;
            }
        }

        if self.is_blocked() {
            writeln!(f)?;
            writeln!(
                f,
                "Advisory findings are not permission. Step 4B may not begin while any blocking \
                 finding stands."
            )?;
        }
        Ok(())
    }
}

/// The registry, in report form.
pub fn classified_tables() -> Vec<ClassifiedTable> {
    inventory::REGISTRY
        .iter()
        .map(|e| ClassifiedTable {
            table: e.table,
            class: e.class,
            row_identity: e.row_identity,
            session_semantics: e.session_semantics,
            canonical_path: e.canonical_path,
            secondary_paths: e.secondary_paths,
            consistency: e.consistency,
            tranche: e.tranche,
            plan: e.plan,
            global_scope_evidence: e.global_scope_evidence,
            rationale: e.rationale,
        })
        .collect()
}

/// One reason code, as it appears in the artifact.
#[derive(Debug, Clone, Serialize)]
pub struct ReasonCodeDoc {
    pub code: &'static str,
    pub severity: Severity,
    pub description: &'static str,
}

/// Everything the digest covers.
///
/// Deliberately a separate type from the artifact: the artifact carries
/// `inventory_digest`, and a digest cannot cover itself. Keeping the payload
/// distinct makes that exclusion structural rather than a field someone has to
/// remember to strip.
#[derive(Debug, Clone, Serialize)]
pub struct CanonicalInventoryPayload {
    pub schema_version: &'static str,
    pub reason_codes: Vec<ReasonCodeDoc>,
    pub tables: Vec<ClassifiedTable>,
}

/// The payload, in the one order everything is rendered in.
///
/// `classified_tables` follows `REGISTRY`, which a test pins to table-name
/// order, and `ReasonCode::ALL` is a fixed list — so the serialization is the
/// same on every run and every machine.
pub fn canonical_inventory_payload() -> CanonicalInventoryPayload {
    CanonicalInventoryPayload {
        schema_version: super::audit::REPORT_SCHEMA_VERSION,
        reason_codes: ReasonCode::ALL
            .iter()
            .map(|c| ReasonCodeDoc {
                code: c.as_str(),
                severity: c.severity(),
                description: c.description(),
            })
            .collect(),
        tables: classified_tables(),
    }
}

/// A SHA-256 digest over the canonical inventory payload.
///
/// The payload excludes `inventory_digest` itself, which is why
/// [`CanonicalInventoryPayload`] is its own type. The digest lets a consumer
/// notice that the tenancy decisions moved, and lets a reviewer see at a glance
/// whether a checked-in artifact was regenerated from the registry it claims to
/// describe.
pub fn inventory_digest() -> String {
    use sha2::{Digest, Sha256};

    let canonical = serde_json::to_string(&canonical_inventory_payload()).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// The checked-in machine artifact.
///
/// Contains the registry and the reason-code catalogue — schema structure and
/// decisions only. No row data, no counts and no timestamp, so the file is
/// byte-stable and a diff in review means a decision changed.
/// Step 4A is a diagnostic surface with no production caller yet: the brief
/// forbids a route, a startup hook and any enforcement, so the tests and
/// Step 4B's migration tooling are its only consumers. Narrow allowance on
/// the entry point rather than the module, so anything that becomes
/// genuinely unreachable still shows up.
#[allow(dead_code)]
pub fn render_inventory_json() -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct Artifact {
        schema_version: &'static str,
        inventory_digest: String,
        reason_codes: Vec<ReasonCodeDoc>,
        tables: Vec<ClassifiedTable>,
    }

    let payload = canonical_inventory_payload();
    let artifact = Artifact {
        schema_version: payload.schema_version,
        // Computed over `payload`, which structurally cannot contain this field.
        inventory_digest: inventory_digest(),
        reason_codes: payload.reason_codes,
        tables: payload.tables,
    };
    serde_json::to_string_pretty(&artifact)
}

/// The checked-in operator artifact.
///
/// Rendered from the same registry as the JSON, so the two cannot drift; a
/// snapshot test regenerates both and compares them with the files on disk.
/// Step 4A is a diagnostic surface with no production caller yet: the brief
/// forbids a route, a startup hook and any enforcement, so the tests and
/// Step 4B's migration tooling are its only consumers. Narrow allowance on
/// the entry point rather than the module, so anything that becomes
/// genuinely unreachable still shows up.
#[allow(dead_code)]
pub fn render_inventory_markdown() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# AEON tenancy inventory — Step 4A");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Generated from `src/tenancy/inventory.rs`. Do not edit by hand — the registry is the \
         source of truth and a test regenerates this file and compares it."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- schema version: `{}`",
        super::audit::REPORT_SCHEMA_VERSION
    );
    let _ = writeln!(out, "- inventory digest: `{}`", inventory_digest());
    let _ = writeln!(out, "- tables classified: {}", inventory::REGISTRY.len());
    let _ = writeln!(out);

    let _ = writeln!(out, "## Classification summary");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Class | Tables |");
    let _ = writeln!(out, "|---|---|");
    for class in [
        TableClass::TenantRoot,
        TableClass::DirectAgentChild,
        TableClass::SessionChild,
        TableClass::MemoryLineageChild,
        TableClass::TenantGlobal,
        TableClass::SystemGlobal,
    ] {
        let tables: Vec<&str> = inventory::REGISTRY
            .iter()
            .filter(|e| e.class == class)
            .map(|e| e.table)
            .collect();
        let rendered = if tables.is_empty() {
            "*(none)*".to_string()
        } else {
            tables
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(out, "| `{}` | {rendered} |", class.as_str());
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "## Session semantics");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Concluded from each table's actual insert paths, update paths, read predicates and "
    );
    let _ = writeln!(
        out,
        "constraints \u{2014} not from whether the column happens to be NULL-able."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "| Table | Session role | Evidence |");
    let _ = writeln!(out, "|---|---|---|");
    for entry in inventory::REGISTRY {
        if let Some(session) = entry.session_semantics {
            let _ = writeln!(
                out,
                "| `{}` | `{}` | {} |",
                entry.table,
                session.role.as_str(),
                session.evidence
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Migration tranches");
    let _ = writeln!(out);
    for tranche in Tranche::ALL {
        let tables: Vec<&str> = inventory::REGISTRY
            .iter()
            .filter(|e| e.tranche == *tranche)
            .map(|e| e.table)
            .collect();
        let _ = writeln!(out, "### {}", tranche.as_str());
        let _ = writeln!(out);
        if tables.is_empty() {
            let _ = writeln!(out, "*(no tables; constraint work only)*");
        } else {
            for table in tables {
                let _ = writeln!(out, "- `{table}`");
            }
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Table detail");
    let _ = writeln!(out);
    for entry in inventory::REGISTRY {
        let _ = writeln!(out, "### `{}`", entry.table);
        let _ = writeln!(out);
        let _ = writeln!(out, "- **class**: `{}`", entry.class.as_str());
        let _ = writeln!(out, "- **tranche**: `{}`", entry.tranche.as_str());
        let _ = writeln!(out, "- **rationale**: {}", entry.rationale);
        let identity: Vec<String> = entry
            .row_identity
            .columns
            .iter()
            .map(|c| format!("`{}` ({})", c.name, c.kind.as_str()))
            .collect();
        let _ = writeln!(out, "- **row identity**: {}", identity.join(", "));
        if let Some(session) = entry.session_semantics {
            let _ = writeln!(
                out,
                "- **session role**: `{}` — {}",
                session.role.as_str(),
                session.evidence
            );
        }
        match &entry.canonical_path {
            Some(path) => {
                let _ = writeln!(out, "- **canonical path**: `{}`", path.label);
            }
            None => {
                let _ = writeln!(out, "- **canonical path**: *(none — SYSTEM_GLOBAL)*");
            }
        }
        if !entry.secondary_paths.is_empty() {
            let _ = writeln!(out, "- **secondary paths**:");
            for path in entry.secondary_paths {
                let _ = writeln!(out, "  - `{}`", path.label);
            }
        }
        if let Some(consistency) = entry.consistency {
            let _ = writeln!(out, "- **consistency**: {consistency}");
        }
        if let Some(evidence) = entry.global_scope_evidence {
            let _ = writeln!(out, "- **global-scope evidence**: {evidence}");
        }
        let _ = writeln!(
            out,
            "- **added columns**: {}",
            list(entry.plan.added_columns)
        );
        let _ = writeln!(
            out,
            "- **initial nullability**: {}",
            entry.plan.initial_nullability
        );
        let _ = writeln!(out, "- **backfill**: `{}`", entry.plan.backfill_source);
        let _ = writeln!(
            out,
            "- **must be zero**: {}",
            list(entry.plan.required_zero_codes)
        );
        let _ = writeln!(
            out,
            "- **pre-validation indexes**: {}",
            list(entry.plan.required_pre_validation_indexes)
        );
        let _ = writeln!(
            out,
            "- **planned composite FKs**: {}",
            list(entry.plan.planned_composite_fks)
        );
        let _ = writeln!(
            out,
            "- **NOT VALID appropriate**: {}",
            entry.plan.not_valid_appropriate
        );
        if let Some(step) = entry.plan.validate_step {
            let _ = writeln!(out, "- **validate step**: {step}");
        }
        if let Some(uniqueness) = entry.plan.future_uniqueness {
            let _ = writeln!(out, "- **future uniqueness**: {uniqueness}");
        }
        let _ = writeln!(out, "- **lock profile**: {}", entry.plan.lock_profile);
        let _ = writeln!(
            out,
            "- **rollback dependencies**: {}",
            list(entry.plan.rollback_dependencies)
        );
        let _ = writeln!(
            out,
            "- **must stay inactive**: {}",
            list(entry.plan.inactive_paths)
        );
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Reason-code catalogue");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Code | Severity | Meaning |");
    let _ = writeln!(out, "|---|---|---|");
    for code in ReasonCode::ALL {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            code.as_str(),
            code.severity().as_str(),
            code.description()
        );
    }
    out
}

fn list(items: &[&str]) -> String {
    if items.is_empty() {
        "*(none)*".to_string()
    } else {
        items
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_is_stable_across_calls() {
        // If this ever flaps, the checked-in artifacts churn on every run and
        // stop meaning "a decision changed".
        assert_eq!(inventory_digest(), inventory_digest());
        assert!(inventory_digest().starts_with("sha256:"));
    }

    #[test]
    fn both_artifacts_render_deterministically() {
        assert_eq!(render_inventory_markdown(), render_inventory_markdown());
        assert_eq!(
            render_inventory_json().unwrap(),
            render_inventory_json().unwrap()
        );
    }

    #[test]
    fn the_markdown_artifact_names_every_table_and_code() {
        let markdown = render_inventory_markdown();
        for entry in inventory::REGISTRY {
            assert!(
                markdown.contains(entry.table),
                "{} missing from the artifact",
                entry.table
            );
        }
        for code in ReasonCode::ALL {
            assert!(
                markdown.contains(code.as_str()),
                "{code} missing from the artifact"
            );
        }
    }

    #[test]
    fn advisory_findings_alone_never_block() {
        let mut report = TenancyAuditReport {
            schema_version: super::super::audit::REPORT_SCHEMA_VERSION,
            generated_at: None,
            inventory_digest: inventory_digest(),
            discovered_application_tables: Vec::new(),
            excluded_objects: Vec::new(),
            classified_tables: Vec::new(),
            blocking_count: 0,
            advisory_count: 7,
            findings: Vec::new(),
            tranche_readiness: Vec::new(),
        };
        // Seven advisory findings and no blocking ones is still READY: advisory
        // output must never be mistaken for a grant, and must never be able to
        // withhold one either.
        assert_eq!(report.verdict(), "READY");

        report.blocking_count = 1;
        assert_eq!(report.verdict(), "BLOCKED");
        assert!(report.is_blocked());
    }
    #[test]
    fn regenerate_artifacts_on_request() {
        // `UPDATE_TENANCY_ARTIFACTS=1 cargo test regenerate_artifacts_on_request`
        // rewrites the checked-in files from the registry. Gated on an
        // environment variable so an ordinary run can never rewrite the very
        // files the snapshot test is checking — a self-updating snapshot proves
        // nothing.
        if std::env::var("UPDATE_TENANCY_ARTIFACTS").as_deref() != Ok("1") {
            return;
        }
        std::fs::write(
            "docs/tenancy/step4a-inventory.json",
            render_inventory_json().unwrap(),
        )
        .unwrap();
        std::fs::write(
            "docs/tenancy/step4a-inventory.md",
            render_inventory_markdown(),
        )
        .unwrap();
    }
}
