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
    self, ExcludedObject, MigrationPlan, OwnershipPath, RowIdentity, SchemaObjectContract,
    SessionSemantics, TableClass, Tranche,
};
use super::plan;

/// What one audit run concluded about one table's declared current-schema
/// contract.
///
/// Per **run**, never per registry entry: the registry states a requirement,
/// and only a run against a particular database can say whether that database
/// meets it. A static artifact therefore carries no status at all rather than a
/// default one — an unconditional `SATISFIED` in a checked-in file would assert
/// something about every deployment, which no file can know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContractStatus {
    /// Structural prerequisites hold and every declared object matches.
    Satisfied,
    /// Verification ran, and one or more required objects differ.
    Drifted,
    /// Fatal table/column/type/identifier drift made safe evaluation
    /// impossible, so the contract was never checked. Deliberately distinct
    /// from `SATISFIED`: "not looked at" must never read as "fine".
    NotEvaluated,
}

impl ContractStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "SATISFIED",
            Self::Drifted => "DRIFTED",
            Self::NotEvaluated => "NOT_EVALUATED",
        }
    }
}

impl std::fmt::Display for ContractStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registry entry as it appears in a report or artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassifiedTable {
    pub table: &'static str,
    pub class: TableClass,
    /// How this table's example row identifier is built. Declared, never
    /// derived from the catalog.
    pub row_identity: RowIdentity,
    /// The schema objects this table's ownership joins and row identity are
    /// *required* to rely on.
    ///
    /// A requirement, not an observation. Every audit run verifies each object
    /// against the live catalog of the database it is pointed at; the outcome
    /// for that run is [`ClassifiedTable::contract_status`]. This list on its
    /// own says nothing about any particular deployment.
    pub required_current_schema_contract: &'static [SchemaObjectContract],
    /// What *this run* found when it verified the contract above.
    ///
    /// `None` in the static artifact, where no run happened — serialized as an
    /// absent field rather than a null or a default, so a checked-in file
    /// cannot be misread as claiming a verification it never performed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_status: Option<ContractStatus>,
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
        writeln!(f, "## Current-schema contract (this run)")?;
        writeln!(f)?;
        for table in &self.classified_tables {
            // Absent status means this rendering did not come from an audit
            // run. Printing SATISFIED here would be the exact overclaim the
            // three-state enum exists to prevent.
            let status = match table.contract_status {
                Some(status) => status.as_str(),
                None => "(no run)",
            };
            writeln!(f, "  {:<34} {status}", table.table)?;
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
            required_current_schema_contract: e.schema_contract,
            // Static rendering: no run, so no status. The audit replaces this
            // with the outcome it measured.
            contract_status: None,
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
    /// The Step 4B contract, covered by the digest so that changing the plan
    /// changes the digest. A consumer that keyed on the digest to detect
    /// "the tenancy decisions moved" would otherwise miss the entire migration
    /// plan moving underneath it.
    pub step_4b_contract: Step4bContract,
}

/// The typed Step 4B plan, as it appears in the artifacts.
#[derive(Debug, Clone, Serialize)]
pub struct Step4bContract {
    /// PREPARE / BACKFILL / FINALIZE, and what stops them running out of order.
    pub stages: Vec<&'static str>,
    pub finalize_guard: &'static str,
    pub concurrent_build_mechanism: &'static str,
    /// Which lock each planned operation actually takes.
    pub lock_profiles: Vec<LockProfileDoc>,
    pub transitional_write_rationale: &'static str,
    pub transitional_write_guarantee: &'static str,
    pub planned_objects: Vec<PlannedObjectDoc>,
    /// Tables Step 4B creates outright, rather than columns added to existing
    /// ones. Currently just the per-tranche backfill checkpoint table, which the
    /// FINALIZE guard asserts against.
    pub planned_tables: Vec<plan::PlannedTable>,
    /// The four conditions FINALIZE checks before validating anything.
    pub finalize_precondition: plan::FinalizePrecondition,
    pub checkpoint_statuses: Vec<&'static str>,
    /// Application changes that must ship and drain before the owning tranche's
    /// PREPARE may be scheduled.
    pub compatibility_prerequisites: Vec<plan::CompatibilityPrerequisite>,
    pub backfill_authority: Vec<plan::BackfillAuthority>,
    /// How each planned unique tuple relates to the one it replaces. No entry
    /// may be a NARROWING without a collision probe.
    pub uniqueness_transitions: Vec<plan::PlannedUniquenessTransition>,
    /// Required-zero codes that cannot currently fire, and why.
    pub structurally_unreachable: Vec<plan::UnreachableCode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockProfileDoc {
    pub operation: &'static str,
    pub locks: &'static str,
}

/// One planned object, rendered from its typed form.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedObjectDoc {
    pub table: &'static str,
    pub name: &'static str,
    /// The tranche this object is filed under, whether or not anything is built.
    pub declared_in: Tranche,
    /// False for a unique target the live schema already provides. Such an
    /// object has no creating tranche and no creation lock, and must never
    /// reach a PREPARE worklist.
    pub requires_creation: bool,
    pub creating_tranche: Option<Tranche>,
    pub validating_tranche: Option<Tranche>,
    pub local_columns: &'static [&'static str],
    pub referenced_table: Option<&'static str>,
    pub referenced_columns: &'static [&'static str],
    pub unique: bool,
    pub not_valid_permitted: bool,
    pub concurrent_build_required: bool,
    pub creation_lock: Option<plan::LockProfile>,
    /// The brief `ADD CONSTRAINT ... USING INDEX` attach, for the unique targets
    /// that are built in two phases. `None` for everything else, including an
    /// already-current target, which is built by nothing.
    pub attachment_lock: Option<plan::LockProfile>,
    pub validation_lock: Option<plan::LockProfile>,
    pub nullability: Option<plan::Nullability>,
    /// Whether a NULL in the local key leaves rows unchecked. `None` for
    /// anything that is not a foreign key.
    pub unenforced_when_null: Option<bool>,
    /// The `MATCH SIMPLE` consequence as a value, so a consumer never has to
    /// substring-match `rendered` to discover it.
    pub match_semantics: Option<plan::MatchSemantics>,
    /// Exactly which local columns can contain NULL, keyed on `(table, column)`
    /// rather than on the column name.
    pub nullable_local_columns: Vec<&'static str>,
    /// The same two facts for the PREPARE-through-BACKFILL window, where an
    /// ownership column is genuinely NULL until its tranche's backfill reaches
    /// the row. A key that is fully enforced in the end state can be enforcing
    /// nothing while the tranche runs.
    pub transition_match_semantics: Option<plan::MatchSemantics>,
    pub transition_nullable_local_columns: Vec<&'static str>,
    pub rendered: String,
}

/// The Step 4B contract, assembled from the typed plan.
pub fn step_4b_contract() -> Step4bContract {
    Step4bContract {
        stages: plan::Stage::ALL.iter().map(|s| s.as_str()).collect(),
        finalize_guard: plan::FINALIZE_GUARD,
        concurrent_build_mechanism: plan::CONCURRENT_BUILD_MECHANISM,
        lock_profiles: plan::LockProfile::ALL
            .iter()
            .map(|l| LockProfileDoc {
                operation: l.as_str(),
                locks: l.locks(),
            })
            .collect(),
        transitional_write_rationale: plan::TRANSITIONAL_WRITE_RATIONALE,
        transitional_write_guarantee: plan::TRANSITIONAL_WRITE_GUARANTEE,
        planned_objects: plan::PLANNED_OBJECTS
            .iter()
            .map(|o| {
                let (referenced_table, referenced_columns) = match o.referenced() {
                    Some((t, c)) => (Some(t), c),
                    None => (None, &[] as &[&str]),
                };
                PlannedObjectDoc {
                    table: o.table(),
                    name: o.name(),
                    declared_in: o.declared_in(),
                    requires_creation: o.requires_creation(),
                    creating_tranche: o.creating_tranche(),
                    validating_tranche: o.validating_tranche(),
                    local_columns: o.local_columns(),
                    referenced_table,
                    referenced_columns,
                    unique: o.is_unique(),
                    not_valid_permitted: o.not_valid_permitted(),
                    concurrent_build_required: o.concurrent_build_required(),
                    creation_lock: o.creation_lock(),
                    attachment_lock: o.attachment_lock(),
                    validation_lock: o.validation_lock(),
                    nullability: o.nullability(),
                    unenforced_when_null: o.unenforced_when_null(),
                    match_semantics: o.match_semantics(),
                    nullable_local_columns: o.nullable_local_columns(),
                    transition_match_semantics: o.transition_match_semantics(),
                    transition_nullable_local_columns: o.transition_nullable_local_columns(),
                    rendered: o.describe(),
                }
            })
            .collect(),
        planned_tables: plan::PLANNED_TABLES.to_vec(),
        finalize_precondition: plan::FINALIZE_PRECONDITION,
        checkpoint_statuses: plan::CheckpointStatus::ALL
            .iter()
            .map(|s| s.as_str())
            .collect(),
        compatibility_prerequisites: plan::COMPATIBILITY_PREREQUISITES.to_vec(),
        backfill_authority: plan::BACKFILL_AUTHORITY.to_vec(),
        uniqueness_transitions: plan::UNIQUENESS_TRANSITIONS.to_vec(),
        structurally_unreachable: plan::UNREACHABLE_REQUIRED_ZERO.to_vec(),
    }
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
        step_4b_contract: step_4b_contract(),
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
        step_4b_contract: Step4bContract,
    }

    let payload = canonical_inventory_payload();
    let artifact = Artifact {
        schema_version: payload.schema_version,
        // Computed over `payload`, which structurally cannot contain this field.
        inventory_digest: inventory_digest(),
        reason_codes: payload.reason_codes,
        tables: payload.tables,
        step_4b_contract: payload.step_4b_contract,
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
    let _ = writeln!(out, "## REQUIRED_CURRENT_SCHEMA_CONTRACT");
    let _ = writeln!(out);
    let _ = writeln!(out, "**RUNTIME CONTRACT VERIFICATION: IMPLEMENTED**");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Each audit execution verifies these declared requirements against its live PostgreSQL \
         catalog."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Each table declares the schema objects its ownership joins and row identity rely on \
         **today** — not the constraints Step 4B will add, which live in the migration plan. \
         Constraints are matched on `contype`, ordered `conkey` attribute names, referenced \
         schema and table, ordered `confkey` attribute names and `convalidated`; indexes on \
         ordered key columns, `indisunique`, `indisvalid`, `indisready`, absence of `indpred` \
         and `indexprs`, and key-column count. A name alone never satisfies a requirement."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "This file is a **static** rendering of the registry. It records what every deployment \
         is required to have, and it is not evidence that any particular deployment has it — \
         only a run against that database can say so, and only for that database. Each run \
         reports `SATISFIED`, `DRIFTED` or `NOT_EVALUATED` per table in its machine report."
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## STEP 4B MIGRATION CONTRACT");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Typed, not prose. Every planned column, index, unique target, foreign key and \
         constraint below is a value in `tenancy::plan::PLANNED_OBJECTS`; this section is \
         generated from it. Invariants check the plan as data — that every planned foreign key \
         has a matching unique target in the same column order, that no key depends on a target \
         created in a later tranche, and that every object has exactly one owning tranche."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "**Nothing here has been created.** Step 4B-0 is the contract; the DDL belongs to Step \
         4B-1 onward."
    );

    let _ = writeln!(out);
    let _ = writeln!(out, "### Three-stage tranche execution");
    let _ = writeln!(out);
    for stage in plan::Stage::ALL {
        let _ = writeln!(out, "1. **{}**", stage.as_str());
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", plan::FINALIZE_GUARD);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", plan::CONCURRENT_BUILD_MECHANISM);

    let _ = writeln!(out);
    let _ = writeln!(out, "### Lock profiles");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Operation | Locks taken |");
    let _ = writeln!(out, "|---|---|");
    for profile in plan::LockProfile::ALL {
        let _ = writeln!(out, "| `{}` | {} |", profile.as_str(), profile.locks());
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "### Transitional write strategy");
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", plan::TRANSITIONAL_WRITE_RATIONALE);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", plan::TRANSITIONAL_WRITE_GUARANTEE);

    let _ = writeln!(out);
    let _ = writeln!(out, "### Planned objects by tranche");
    let _ = writeln!(out);
    for tranche in Tranche::ALL {
        let objects = plan::planned_in(*tranche);
        if objects.is_empty() {
            continue;
        }
        let _ = writeln!(out, "#### {}", tranche.as_str());
        let _ = writeln!(out);
        // PREPARE builds only what needs creating. An already-current target is
        // declared under a tranche but built by nothing, and emitting DDL for it
        // would fail against the object that already exists.
        let worklist = plan::prepare_worklist(*tranche);
        let _ = writeln!(
            out,
            "PREPARE builds {} of the {} objects declared here.",
            worklist.len(),
            objects.len()
        );
        for blocker in plan::prepare_blockers(*tranche) {
            let _ = writeln!(
                out,
                "- **PREPARE blocked**: `{}` — {}",
                blocker.planned_object, blocker.blocker
            );
        }
        let _ = writeln!(out);
        for object in objects {
            let _ = writeln!(
                out,
                "- {}{}",
                object.describe(),
                if object.requires_creation() {
                    ""
                } else {
                    " *(nothing is built; retained only as a dependency prerequisite)*"
                }
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "### Uniqueness transitions");
    let _ = writeln!(out);
    for shape in plan::UniquenessTransition::ALL {
        let planned: Vec<&str> = plan::UNIQUENESS_TRANSITIONS
            .iter()
            .filter(|t| t.shape() == *shape)
            .map(|t| t.table)
            .collect();
        let _ = writeln!(
            out,
            "- `{}` — {}{}",
            shape.as_str(),
            if planned.is_empty() {
                "none planned".to_string()
            } else {
                planned
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            if shape.can_collide() {
                " *(requires a collision probe before creation)*"
            } else {
                ""
            }
        );
    }
    let _ = writeln!(out);
    for transition in plan::UNIQUENESS_TRANSITIONS {
        let _ = writeln!(
            out,
            "- `{}`: ({}) → ({}) — **{}**. {}",
            transition.table,
            transition.from.join(", "),
            transition.to.join(", "),
            transition.shape().as_str(),
            transition.reason
        );
        if let Some(probe) = transition.collision_probe() {
            let _ = writeln!(out, "  - **collision probe**: `{probe}`");
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "### Structurally unreachable required-zero codes");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Recorded rather than silently dropped. The enum values stay — they are a stable \
         contract — but these table/code pairs cannot currently fire, so leaving them in a \
         required-zero set would make a gate look stricter than it is."
    );
    let _ = writeln!(out);
    for unreachable in plan::UNREACHABLE_REQUIRED_ZERO {
        let _ = writeln!(
            out,
            "- `{}` / `{}` — {}",
            unreachable.table,
            unreachable.code.as_str(),
            unreachable.reason
        );
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
        let _ = writeln!(
            out,
            "- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every \
             audit run; per-run status is in the machine report, not here.*"
        );
        if entry.schema_contract.is_empty() {
            let _ = writeln!(out, "  - *(none)*");
        } else {
            for object in entry.schema_contract {
                let _ = writeln!(out, "  - {}", object.describe());
            }
        }
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
        if let Some(transition) = plan::uniqueness_transition_for(entry.table) {
            let _ = writeln!(
                out,
                "- **uniqueness transition**: `{}` — ({}) → ({}). {}",
                transition.shape().as_str(),
                transition.from.join(", "),
                transition.to.join(", "),
                transition.reason
            );
        }
        if let Some(evidence) = entry.global_scope_evidence {
            let _ = writeln!(out, "- **global-scope evidence**: {evidence}");
        }
        // Planned objects are rendered from the typed plan, never from a second
        // prose copy kept beside it.
        let planned = plan::planned_for(entry.table);
        if planned.is_empty() {
            let _ = writeln!(
                out,
                "- **planned objects**: *(none — this table owes no DDL)*"
            );
        } else {
            let _ = writeln!(out, "- **planned objects**:");
            for object in &planned {
                let _ = writeln!(
                    out,
                    "  - [{}] {}",
                    object.declared_in().as_str(),
                    object.describe()
                );
                // The MATCH consequence is emitted as a labelled value, so a
                // consumer reading this file never has to parse the sentence
                // above it.
                if let Some(semantics) = object.match_semantics() {
                    let nullable = object.nullable_local_columns();
                    let _ = writeln!(
                        out,
                        "    - **match semantics**: `{}`{}",
                        semantics.as_str(),
                        if nullable.is_empty() {
                            String::new()
                        } else {
                            format!(" — NULL-able local columns: `{}`", nullable.join("`, `"))
                        }
                    );
                }
            }
        }
        if let Some(strategy) = plan::transition_for(entry.table) {
            let _ = writeln!(
                out,
                "- **transitional writes**: `{}`{}",
                strategy.as_str(),
                match strategy {
                    plan::TransitionalWrite::ConditionalAgentBridge {
                        trigger, reason, ..
                    } => format!(
                        " — `{trigger}` installed in PREPARE, before any backfill; resolves \
                         conditionally ({}) — {reason}",
                        plan::ConditionalOwnership::ALL
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    plan::TransitionalWrite::MultiPathBridge {
                        trigger, parents, ..
                    } => format!(
                        " — `{trigger}` installed in PREPARE, before any backfill; writes only \
                         when {} agree, and never picks a side",
                        parents.join(" and ")
                    ),
                    other => format!(
                        " — `{}` (`{}`) installed in PREPARE, before any backfill{}",
                        other.trigger(),
                        other.function(),
                        if other.resolves_through_a_parent() {
                            ", resolving through the parent its backfill authority names"
                        } else {
                            ""
                        }
                    ),
                }
            );
        }
        if let Some(authority) = plan::backfill_authority_for(entry.table) {
            let _ = writeln!(out, "- **backfill source**: `{}`", authority.source);
            if let Some(agreement) = authority.agreement {
                let _ = writeln!(out, "- **all paths must agree**: `{agreement}`");
            }
            let _ = writeln!(out, "- **on disagreement**: {}", authority.on_disagreement);
        }
        let _ = writeln!(
            out,
            "- **initial nullability**: {}",
            entry.plan.initial_nullability
        );
        let _ = writeln!(
            out,
            "- **backfill shape**: `{}`",
            entry.plan.backfill_source
        );
        let _ = writeln!(
            out,
            "- **must be zero**: {}",
            list(&codes(entry.plan.required_zero_codes))
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

/// Reason codes as their contract strings, for rendering only.
fn codes(items: &[ReasonCode]) -> Vec<&'static str> {
    items.iter().map(|c| c.as_str()).collect()
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

    /// The published lock-profile table lists every profile the plan defines.
    ///
    /// Both the JSON block and the Markdown table are built by mapping
    /// `LockProfile::ALL`, so they cannot drift from the enum — but a filter
    /// added to either would silently publish a shorter contract, and a reader
    /// cannot tell a profile that was omitted from one that does not exist.
    #[test]
    fn every_lock_profile_reaches_the_published_contract() {
        let published = step_4b_contract().lock_profiles;
        assert_eq!(published.len(), plan::LockProfile::ALL.len());
        for profile in plan::LockProfile::ALL {
            assert!(
                published.iter().any(|p| p.operation == profile.as_str()),
                "`{}` is defined but never published",
                profile.as_str()
            );
        }
        // The two-phase build's second half is the reason this matters: it was
        // the profile most recently missing from the rendered contract.
        assert!(published
            .iter()
            .any(|p| p.operation == plan::LockProfile::AddUniqueUsingIndex.as_str()));
    }

    /// The schema version is pinned to a literal.
    ///
    /// Comparing `report.schema_version` against `REPORT_SCHEMA_VERSION` — as
    /// the report tests do — is comparing the field with the constant it was
    /// assigned from, which passes for any string. Step 4B-0 changed the
    /// serialized shape, so a consumer pinned to `step4a.1` must be refused;
    /// that only holds if the value is actually the new one.
    ///
    /// Bumped to `step4b0.2` when planned objects gained `attachment_lock` and
    /// `LockProfile` gained `AddUniqueUsingIndex`. Both are reachable by a
    /// consumer pinned to `step4b0.1`, so the addition is not backward
    /// compatible in the way a purely additive field would be.
    #[test]
    fn the_report_schema_version_is_pinned_to_the_step_4b0_shape() {
        assert_eq!(
            super::super::audit::REPORT_SCHEMA_VERSION,
            "step4b0.2",
            "the Step 4B-0 contract changed the payload shape; reverting the version would let a \
             Step 4A consumer read a report it cannot understand"
        );
        assert_eq!(
            canonical_inventory_payload().schema_version,
            "step4b0.2",
            "the payload must carry the bumped version, not merely define it"
        );
    }

    /// The typed null semantics reach `PlannedObjectDoc` intact.
    ///
    /// The artifact snapshot cannot catch a mapping bug here: it is generated
    /// by, and compared against, this same code path, so a swapped or dropped
    /// field would be baked into both sides at once. These expectations are
    /// stated independently of the mapping.
    #[test]
    fn planned_object_docs_carry_the_typed_null_semantics() {
        let contract = step_4b_contract();
        let doc = |name: &str| {
            contract
                .planned_objects
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("`{name}` must appear in the contract"))
        };

        // A foreign key whose local key contains a pre-existing NULL-able
        // column: MATCH SIMPLE leaves those rows unchecked.
        let unchecked = doc("memories_archival_batch_tenant_fkey");
        assert_eq!(unchecked.unenforced_when_null, Some(true));
        assert_eq!(
            unchecked.match_semantics,
            Some(plan::MatchSemantics::NullKeyRowsUnchecked)
        );
        assert_eq!(unchecked.nullable_local_columns, vec!["archival_batch_id"]);

        // A foreign key whose local columns are all NOT NULL once created.
        let checked = doc("memory_versions_memory_tenant_fkey");
        assert_eq!(checked.unenforced_when_null, Some(false));
        assert_eq!(
            checked.match_semantics,
            Some(plan::MatchSemantics::AllRowsChecked)
        );
        assert!(checked.nullable_local_columns.is_empty());

        // A non-foreign-key object has no MATCH question to answer.
        let index = doc("idx_memories_tenant");
        assert_eq!(index.unenforced_when_null, None);
        assert_eq!(index.match_semantics, None);

        // Every doc must agree with itself about whether anything is created.
        for doc in &contract.planned_objects {
            assert_eq!(
                doc.requires_creation,
                doc.creating_tranche.is_some(),
                "`{}` disagrees with itself about being created",
                doc.name
            );
            assert_eq!(
                doc.requires_creation,
                doc.creation_lock.is_some(),
                "`{}` disagrees with itself about taking a creation lock",
                doc.name
            );
            // An attach phase only exists for something that is built, and it
            // is always the two-phase unique-target build.
            if let Some(attach) = doc.attachment_lock {
                assert!(
                    doc.requires_creation,
                    "`{}` publishes an attach lock for something nothing builds",
                    doc.name
                );
                assert_eq!(attach, plan::LockProfile::AddUniqueUsingIndex);
            }
            // …and the derived flag must match the columns it was derived from.
            assert_eq!(
                doc.unenforced_when_null,
                doc.match_semantics
                    .map(|s| s == plan::MatchSemantics::NullKeyRowsUnchecked),
                "`{}` disagrees with itself about MATCH semantics",
                doc.name
            );
            if doc.unenforced_when_null == Some(true) {
                assert!(
                    !doc.nullable_local_columns.is_empty(),
                    "`{}` is marked unenforced but names no NULL-able column",
                    doc.name
                );
            }
        }
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
