//! Vault-to-vault replication, the CouchDB way. `pull` copies every
//! revision of a source vault that this vault does not have. `sync` is a
//! pull in each direction. Revision ids are content hashes, so replay is a
//! no-op and both vaults converge on the same winner per document.
//!
//! The source's change feed is in commit order, and a parent always
//! commits before its child, so parents arrive first. There is no
//! revision-diff step.
//!
//! Every document replicates. Local state lives in tables outside `docs`
//! (`task_state`, `vault`, `checkpoints`) and never does: a task that
//! arrives is dormant until this vault enables it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::store::{MAX_LIMIT, Store};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PullReport {
    /// The source vault.
    pub peer: String,
    /// Revisions read from the source's change feed.
    pub read: usize,
    /// Revisions this vault did not have, now written.
    pub written: usize,
    /// Revisions this vault already had.
    pub present: usize,
    /// Revisions skipped because an ancestor did not replicate.
    pub missing_parent: usize,
    /// The source's change-feed position, now this vault's checkpoint.
    pub last_seq: i64,
    /// True when the checkpoint did not match the source, so the pull
    /// read the source's whole feed again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restarted: bool,
}

/// Copy into `target` every revision of `source` committed after the
/// checkpoint for `peer`. `peer` names the source, stably: the CLI uses
/// its canonical path.
pub fn pull(target: &mut Store, source: &Store, peer: &str) -> Result<PullReport, StoreError> {
    let mut report = PullReport { peer: peer.to_string(), ..Default::default() };
    let mut since = match target.checkpoint(peer)? {
        None => 0,
        Some((seq, rev)) if source.rev_at_seq(seq)?.as_deref() == Some(rev.as_str()) => seq,
        Some(_) => {
            report.restarted = true;
            0
        }
    };
    loop {
        let batch = source.changes(since, Some(MAX_LIMIT))?;
        let Some(last) = batch.results.last() else { break };
        let last_rev = last.rev.clone();
        report.read += batch.results.len();
        let applied = target.apply_replicas(peer, &batch.results, batch.last_seq, &last_rev)?;
        report.written += applied.written;
        report.present += applied.present;
        report.missing_parent += applied.missing_parent;
        since = batch.last_seq;
    }
    report.last_seq = since;
    Ok(report)
}

/// Pull `b` into `a`, then `a` into `b`. After a sync without concurrent
/// writers, both vaults hold the same revisions.
pub fn sync(a: &mut Store, a_peer: &str, b: &mut Store, b_peer: &str) -> Result<[PullReport; 2], StoreError> {
    let into_a = pull(a, b, b_peer)?;
    let into_b = pull(b, a, a_peer)?;
    Ok([into_a, into_b])
}
