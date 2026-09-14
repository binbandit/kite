//! Durable rollback records and atomic Git ref updates.
//!
//! A land moves from Landing to Completed; undo moves through Undoing to Empty.
//! The old commits remain reachable until the corresponding transition finishes.

use crate::git::{check_ref, config_get, config_unset, execute_git, execute_git_with};
use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};

/// Where the pre-land `HEAD` is parked so `kt undo` can restore it, plus the
/// local config keys recording which branch that land belongs to — or
/// `DETACHED_TARGET` when it was landed on a detached `HEAD` — where it left
/// `HEAD`, and which linked worktree owns the marker.
pub(super) const PRE_LAND_REF: &str = "refs/kite/pre_land";
pub(super) const PRE_LAND_BRANCH_KEY: &str = "kite.preland.branch";
pub(super) const PRE_LAND_HEAD_KEY: &str = "kite.preland.head";
pub(super) const PRE_LAND_WORKTREE_KEY: &str = "kite.preland.worktree";
/// Atomic, authoritative rollback metadata. The ref points to a JSON blob so
/// every field changes in one compare-and-swap ref transaction.
pub(super) const LAND_STATE_REF: &str = "refs/kite/land_state";
pub(super) const LAND_STATE_VERSION: u8 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CompletedLand {
    pub(super) pre_land_sha: String,
    pub(super) target: String,
    pub(super) owner: Option<String>,
    pub(super) landed_head: String,
    pub(super) keepalive_ref: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum StableLand {
    Empty,
    Completed { land: CompletedLand },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct LandingLand {
    pub(super) pre_land_sha: String,
    pub(super) target: String,
    pub(super) owner: String,
    pub(super) transaction_ref: String,
    pub(super) keepalive_ref: String,
    pub(super) previous: StableLand,
}

/// One immutable object containing the whole rollback transaction. Updating a
/// ref to this blob with an expected old oid both serializes linked worktrees
/// and prevents crashes from exposing a mixture of old and new fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct AtomicLandRecord {
    pub(super) version: u8,
    #[serde(flatten)]
    pub(super) phase: AtomicLandPhase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(super) enum AtomicLandPhase {
    Empty,
    Landing {
        land: LandingLand,
    },
    Completed {
        land: CompletedLand,
    },
    Undoing {
        land: CompletedLand,
        owner: String,
        from_head: String,
    },
}

// Legacy completed markers lack an atomic state id and a keepalive ref.
#[derive(Clone, Debug)]
pub(super) struct CompletedMarker {
    pub(super) state_oid: Option<String>,
    pub(super) pre_land_sha: String,
    pub(super) target: String,
    pub(super) owner: Option<String>,
    pub(super) landed_head: String,
    pub(super) keepalive_ref: Option<String>,
}

#[derive(Debug)]
pub(super) enum PreLandState {
    Empty { state_oid: Option<String> },
    Completed(CompletedMarker),
    InProgress(LandTransaction),
    Undoing(UndoTransaction),
    LegacyInProgress { owner: String },
    Inconsistent,
}

impl PreLandState {
    pub(super) fn recovery_owner(&self) -> Option<&str> {
        match self {
            Self::InProgress(transaction) => Some(&transaction.landing.owner),
            Self::Undoing(transaction) => Some(&transaction.owner),
            Self::LegacyInProgress { owner } => Some(owner),
            _ => None,
        }
    }
}

pub(super) fn pre_land_state() -> PreLandState {
    match read_atomic_land_record() {
        Ok(Some((state_oid, AtomicLandPhase::Empty))) => PreLandState::Empty {
            state_oid: Some(state_oid),
        },
        Ok(Some((state_oid, AtomicLandPhase::Completed { land }))) => {
            PreLandState::Completed(CompletedMarker {
                state_oid: Some(state_oid),
                pre_land_sha: land.pre_land_sha,
                target: land.target,
                owner: land.owner,
                landed_head: land.landed_head,
                keepalive_ref: Some(land.keepalive_ref),
            })
        }
        Ok(Some((state_oid, AtomicLandPhase::Landing { land }))) => {
            PreLandState::InProgress(LandTransaction {
                state_oid,
                landing: land,
            })
        }
        Ok(Some((
            state_oid,
            AtomicLandPhase::Undoing {
                land,
                owner,
                from_head,
            },
        ))) => PreLandState::Undoing(UndoTransaction {
            state_oid,
            land,
            owner,
            from_head,
        }),
        Ok(None) => read_legacy_land_state(),
        Err(_) => PreLandState::Inconsistent,
    }
}

pub(super) fn read_legacy_land_state() -> PreLandState {
    let sha = check_ref(PRE_LAND_REF);
    let target = config_get(PRE_LAND_BRANCH_KEY);
    let landed_head = config_get(PRE_LAND_HEAD_KEY);
    let owner = config_get(PRE_LAND_WORKTREE_KEY);
    match (sha, target, landed_head, owner) {
        // Old versions could leave a bare rollback ref with no land metadata.
        (_, None, None, None) => PreLandState::Empty { state_oid: None },
        (Some(pre_land_sha), Some(target), Some(landed_head), owner) => {
            PreLandState::Completed(CompletedMarker {
                state_oid: None,
                pre_land_sha,
                target,
                owner,
                landed_head,
                keepalive_ref: None,
            })
        }
        (Some(_), Some(_), None, Some(owner)) => PreLandState::LegacyInProgress { owner },
        _ => PreLandState::Inconsistent,
    }
}

pub(super) fn read_atomic_land_record() -> Result<Option<(String, AtomicLandPhase)>> {
    let Some(state_oid) = check_ref(LAND_STATE_REF) else {
        return Ok(None);
    };
    let json = execute_git(&["cat-file", "blob", &state_oid])
        .context("Could not read Kite's atomic land marker")?;
    let record: AtomicLandRecord =
        serde_json::from_str(&json).context("Kite's atomic land marker is not valid JSON")?;

    if record.version != LAND_STATE_VERSION || !valid_atomic_phase(&record.phase) {
        anyhow::bail!("Kite's atomic land marker has an unsupported or incomplete shape");
    }

    Ok(Some((state_oid, record.phase)))
}

pub(super) fn valid_atomic_phase(phase: &AtomicLandPhase) -> bool {
    let valid_completed = |land: &CompletedLand| {
        !land.pre_land_sha.is_empty()
            && !land.target.is_empty()
            && !land.landed_head.is_empty()
            && land.keepalive_ref.starts_with("refs/kite/keepalive/")
    };

    match phase {
        AtomicLandPhase::Empty => true,
        AtomicLandPhase::Completed { land } => valid_completed(land),
        AtomicLandPhase::Undoing {
            land,
            owner,
            from_head,
        } => valid_completed(land) && !owner.is_empty() && !from_head.is_empty(),
        AtomicLandPhase::Landing { land } => {
            !land.pre_land_sha.is_empty()
                && !land.target.is_empty()
                && !land.owner.is_empty()
                && land.transaction_ref.starts_with(TRANSACTION_REF_PREFIX)
                && land.keepalive_ref.starts_with("refs/kite/keepalive/")
                && match &land.previous {
                    StableLand::Empty => true,
                    StableLand::Completed { land } => valid_completed(land),
                }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct UndoTransaction {
    pub(super) state_oid: String,
    pub(super) land: CompletedLand,
    pub(super) owner: String,
    pub(super) from_head: String,
}

pub(super) fn unique_kite_ref(prefix: &str) -> String {
    format!(
        "{prefix}{}-{}-{}",
        Local::now().format("%Y%m%d%H%M%S"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0)
    )
}

pub(super) const TRANSACTION_REF_PREFIX: &str = "refs/heads/kite-landing-";

pub(super) fn record_landed_head(transaction: &LandTransaction, landed_head: &str) -> Result<()> {
    #[cfg(test)]
    if FAIL_LANDED_HEAD_WRITE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("injected landed-head marker failure");
    }

    let completed_land = CompletedLand {
        pre_land_sha: transaction.landing.pre_land_sha.clone(),
        target: transaction.landing.target.clone(),
        owner: Some(transaction.landing.owner.clone()),
        landed_head: landed_head.to_string(),
        keepalive_ref: transaction.landing.keepalive_ref.clone(),
    };
    let completed = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: AtomicLandPhase::Completed {
            land: completed_land,
        },
    };
    let completed_oid = write_land_record(&completed)?;
    let transaction_tip = check_ref(&transaction.landing.transaction_ref)
        .context("Kite's land transaction ref disappeared before completion")?;
    if transaction_tip != landed_head {
        anyhow::bail!(
            "Kite's land transaction ref moved unexpectedly; refusing to record a different landed HEAD"
        );
    }

    let mut edits = vec![
        RefEdit::Update {
            name: LAND_STATE_REF.to_string(),
            new: completed_oid,
            old: transaction.state_oid.clone(),
        },
        RefEdit::Delete {
            name: transaction.landing.transaction_ref.clone(),
            old: landed_head.to_string(),
        },
    ];
    if let StableLand::Completed { land } = &transaction.landing.previous {
        edits.push(RefEdit::Delete {
            name: land.keepalive_ref.clone(),
            old: land.pre_land_sha.clone(),
        });
    }
    commit_ref_transaction(&edits)?;
    clear_legacy_marker_config();
    Ok(())
}

#[cfg(test)]
pub(super) static FAIL_LANDED_HEAD_WRITE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn install_in_progress_marker(
    previous: &PreLandMarker,
    pre_land_sha: &str,
    target: &str,
    worktree: &str,
    transaction_ref: &str,
) -> Result<LandTransaction> {
    let transaction_id = transaction_ref
        .strip_prefix(TRANSACTION_REF_PREFIX)
        .context("Kite generated an invalid land transaction ref")?;
    let keepalive_ref = format!("refs/kite/keepalive/{transaction_id}");
    let previous_keepalive_ref = format!("{keepalive_ref}-previous");
    let (stable, create_previous_keepalive) = previous.stable(&previous_keepalive_ref)?;

    let landing = LandingLand {
        pre_land_sha: pre_land_sha.to_string(),
        target: target.to_string(),
        owner: worktree.to_string(),
        transaction_ref: transaction_ref.to_string(),
        keepalive_ref: keepalive_ref.clone(),
        previous: stable,
    };
    let state = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: AtomicLandPhase::Landing {
            land: landing.clone(),
        },
    };
    let state_oid = write_land_record(&state)?;

    let mut edits = Vec::new();
    edits.push(match previous.state_oid.as_deref() {
        Some(old) => RefEdit::Update {
            name: LAND_STATE_REF.to_string(),
            new: state_oid.clone(),
            old: old.to_string(),
        },
        None => RefEdit::Create {
            name: LAND_STATE_REF.to_string(),
            new: state_oid.clone(),
        },
    });
    edits.push(RefEdit::Create {
        name: transaction_ref.to_string(),
        new: pre_land_sha.to_string(),
    });
    edits.push(RefEdit::Create {
        name: keepalive_ref,
        new: pre_land_sha.to_string(),
    });
    if let Some(previous_sha) = create_previous_keepalive {
        edits.push(RefEdit::Create {
            name: previous_keepalive_ref,
            new: previous_sha,
        });
    }
    edits.push(match previous.sha.as_deref() {
        Some(old) => RefEdit::Update {
            name: PRE_LAND_REF.to_string(),
            new: pre_land_sha.to_string(),
            old: old.to_string(),
        },
        None => RefEdit::Create {
            name: PRE_LAND_REF.to_string(),
            new: pre_land_sha.to_string(),
        },
    });

    commit_ref_transaction(&edits)
        .context("Could not install rollback state; no history changed")?;
    clear_legacy_marker_config();

    Ok(LandTransaction { state_oid, landing })
}

/// The rollback marker as it stood before a land began.
pub(super) struct PreLandMarker {
    pub(super) state_oid: Option<String>,
    pub(super) sha: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) head: Option<String>,
    pub(super) worktree: Option<String>,
    pub(super) recorded: Option<CompletedMarker>,
}

impl PreLandMarker {
    pub(super) fn capture() -> Self {
        let state = pre_land_state();
        let (state_oid, recorded) = match state {
            PreLandState::Empty { state_oid } => (state_oid, None),
            PreLandState::Completed(recorded) => (recorded.state_oid.clone(), Some(recorded)),
            PreLandState::InProgress(_)
            | PreLandState::Undoing(_)
            | PreLandState::LegacyInProgress { .. }
            | PreLandState::Inconsistent => (None, None),
        };
        Self {
            state_oid,
            sha: check_ref(PRE_LAND_REF),
            branch: config_get(PRE_LAND_BRANCH_KEY),
            head: config_get(PRE_LAND_HEAD_KEY),
            worktree: config_get(PRE_LAND_WORKTREE_KEY),
            recorded,
        }
    }

    fn stable(&self, legacy_keepalive_ref: &str) -> Result<(StableLand, Option<String>)> {
        let Some(recorded) = &self.recorded else {
            // A bare `pre_land` pointer is not a marker either: with nothing
            // recorded beside it there is no land to carry forward, and the
            // install that follows overwrites the pointer itself.
            if self.branch.is_none() && self.head.is_none() && self.worktree.is_none() {
                return Ok((StableLand::Empty, None));
            }
            anyhow::bail!("Kite's previous rollback marker is incomplete");
        };

        let landed_head = recorded.landed_head.clone();
        let (keepalive_ref, create_keepalive) = match &recorded.keepalive_ref {
            Some(existing) => (existing.clone(), None),
            None => (
                legacy_keepalive_ref.to_string(),
                Some(recorded.pre_land_sha.clone()),
            ),
        };
        Ok((
            StableLand::Completed {
                land: CompletedLand {
                    pre_land_sha: recorded.pre_land_sha.clone(),
                    target: recorded.target.clone(),
                    owner: recorded.owner.clone(),
                    landed_head,
                    keepalive_ref,
                },
            },
            create_keepalive,
        ))
    }
}

#[derive(Clone, Debug)]
pub(super) struct LandTransaction {
    pub(super) state_oid: String,
    pub(super) landing: LandingLand,
}

#[derive(Clone, Debug)]
pub(super) enum RefEdit {
    Create {
        name: String,
        new: String,
    },
    Update {
        name: String,
        new: String,
        old: String,
    },
    Delete {
        name: String,
        old: String,
    },
}

pub(super) fn write_land_record(record: &AtomicLandRecord) -> Result<String> {
    let json = serde_json::to_string(record).context("Could not serialize Kite's land marker")?;
    let oid = execute_git_with(&["hash-object", "-w", "--stdin"], &[], Some(&json))?;
    let oid = oid.trim();
    if oid.is_empty() {
        anyhow::bail!("Git did not return an object id for Kite's land marker");
    }
    Ok(oid.to_string())
}

pub(super) fn commit_ref_transaction(edits: &[RefEdit]) -> Result<()> {
    let mut input = String::from("start\n");
    for edit in edits {
        match edit {
            RefEdit::Create { name, new } => {
                input.push_str(&format!("create {name} {new}\n"));
            }
            RefEdit::Update { name, new, old } => {
                input.push_str(&format!("update {name} {new} {old}\n"));
            }
            RefEdit::Delete { name, old } => {
                input.push_str(&format!("delete {name} {old}\n"));
            }
        }
    }
    input.push_str("prepare\ncommit\n");
    execute_git_with(&["update-ref", "--stdin"], &[], Some(&input)).map(|_| ())
}

pub(super) fn restore_previous_marker(transaction: &LandTransaction) -> Result<()> {
    let previous_phase = match &transaction.landing.previous {
        StableLand::Empty => AtomicLandPhase::Empty,
        StableLand::Completed { land } => {
            if check_ref(&land.keepalive_ref).as_deref() != Some(&land.pre_land_sha) {
                anyhow::bail!("The previous land's keepalive ref moved; recovery stopped safely");
            }
            AtomicLandPhase::Completed { land: land.clone() }
        }
    };
    let previous_state = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: previous_phase,
    };
    let previous_state_oid = write_land_record(&previous_state)?;
    let mut edits = vec![RefEdit::Update {
        name: LAND_STATE_REF.to_string(),
        new: previous_state_oid,
        old: transaction.state_oid.clone(),
    }];

    if let Some(transaction_tip) = check_ref(&transaction.landing.transaction_ref) {
        edits.push(RefEdit::Delete {
            name: transaction.landing.transaction_ref.clone(),
            old: transaction_tip,
        });
    }
    if check_ref(&transaction.landing.keepalive_ref).as_deref()
        != Some(&transaction.landing.pre_land_sha)
    {
        anyhow::bail!("Kite's current land keepalive ref moved; recovery stopped safely");
    }
    edits.push(RefEdit::Delete {
        name: transaction.landing.keepalive_ref.clone(),
        old: transaction.landing.pre_land_sha.clone(),
    });

    let current_pre_land = check_ref(PRE_LAND_REF);
    let previous_pre_land = match &transaction.landing.previous {
        StableLand::Empty => None,
        StableLand::Completed { land } => Some(land.pre_land_sha.clone()),
    };
    match (current_pre_land, previous_pre_land) {
        (Some(current), Some(previous))
            if current == transaction.landing.pre_land_sha || current == previous =>
        {
            edits.push(RefEdit::Update {
                name: PRE_LAND_REF.to_string(),
                new: previous,
                old: current,
            });
        }
        (None, Some(previous)) => edits.push(RefEdit::Create {
            name: PRE_LAND_REF.to_string(),
            new: previous,
        }),
        (Some(current), None) if current == transaction.landing.pre_land_sha => {
            edits.push(RefEdit::Delete {
                name: PRE_LAND_REF.to_string(),
                old: current,
            });
        }
        (None, None) => {}
        _ => anyhow::bail!("Kite's rollback ref moved during recovery; it was left untouched"),
    }

    commit_ref_transaction(&edits)?;
    clear_legacy_marker_config();
    Ok(())
}

pub(super) fn clear_legacy_marker_config() {
    let _ = config_unset(PRE_LAND_BRANCH_KEY);
    let _ = config_unset(PRE_LAND_HEAD_KEY);
    let _ = config_unset(PRE_LAND_WORKTREE_KEY);
}
