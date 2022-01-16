//! The Raft storage interface and data types.

use std::fmt::Debug;
use std::ops::RangeBounds;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncSeek;
use tokio::io::AsyncWrite;

use crate::core::EffectiveMembership;
use crate::defensive::check_range_matches_entries;
use crate::raft::Entry;
use crate::raft::EntryPayload;
use crate::raft_types::SnapshotId;
use crate::raft_types::StateMachineChanges;
use crate::AppData;
use crate::AppDataResponse;
use crate::LogId;
use crate::NodeId;
use crate::StorageError;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    // Log entries upto which this snapshot includes, inclusive.
    pub last_log_id: LogId,

    /// To identify a snapshot when transferring.
    /// Caveat: even when two snapshot is built with the same `last_log_id`, they still could be different in bytes.
    pub snapshot_id: SnapshotId,
}

/// The data associated with the current snapshot.
#[derive(Debug)]
pub struct Snapshot<S>
where S: AsyncRead + AsyncSeek + Send + Unpin + 'static
{
    /// metadata of a snapshot
    pub meta: SnapshotMeta,

    /// A read handle to the associated snapshot.
    pub snapshot: Box<S>,
}

/// A record holding the hard state of a Raft node.
///
/// This model derives serde's traits for easily (de)serializing this
/// model for storage & retrieval.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct HardState {
    /// The last recorded term observed by this system.
    pub current_term: u64,
    /// The ID of the node voted for in the `current_term`.
    pub voted_for: Option<NodeId>,
}

/// A struct used to represent the initial state which a Raft node needs when first starting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitialState {
    /// The last entry.
    pub last_log_id: Option<LogId>,

    /// The LogId of the last log applied to the state machine.
    pub last_applied: Option<LogId>,

    /// The saved hard state of the node.
    pub hard_state: HardState,

    /// The latest cluster membership configuration found, in log or in state machine, else a new initial
    /// membership config consisting only of this node's ID.
    pub last_membership: Option<EffectiveMembership>,
}

/// A trait defining the interface for a Raft storage system.
///
/// See the [storage chapter of the guide](https://datafuselabs.github.io/openraft/storage.html)
/// for details and discussion on this trait and how to implement it.
#[async_trait]
pub trait RaftStorage<D, R>: Send + Sync + 'static
where
    D: AppData,
    R: AppDataResponse,
{
    // TODO(xp): simplify storage API

    /// The storage engine's associated type used for exposing a snapshot for reading & writing.
    ///
    /// See the [storage chapter of the guide](https://datafuselabs.github.io/openraft/getting-started.html#implement-raftstorage)
    /// for details on where and how this is used.
    type SnapshotData: AsyncRead + AsyncWrite + AsyncSeek + Send + Sync + Unpin + 'static;

    /// Returns the last membership config found in log or state machine.
    async fn get_membership(&self) -> Result<Option<EffectiveMembership>, StorageError> {
        let (_, sm_mem) = self.last_applied_state().await?;

        let sm_mem_index = match &sm_mem {
            None => 0,
            Some(mem) => mem.log_id.index,
        };

        let log_mem = self.last_membership_in_log(sm_mem_index + 1).await?;

        if log_mem.is_some() {
            return Ok(log_mem);
        }

        return Ok(sm_mem);
    }

    /// Get the latest membership config found in the log.
    ///
    /// This method should returns membership with the greatest log index which is `>=since_index`.
    /// If no such membership log is found, it returns `None`, e.g., when logs are cleaned after being applied.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn last_membership_in_log(&self, since_index: u64) -> Result<Option<EffectiveMembership>, StorageError> {
        let (first_log_id, last_log_id) = self.get_log_state().await?;

        let first_log_id = match first_log_id {
            None => {
                // There is no log at all
                return Ok(None);
            }
            Some(x) => x,
        };

        let mut end = last_log_id.unwrap().index + 1;
        let start = std::cmp::max(first_log_id.index, since_index);
        let step = 64;

        while start < end {
            let entries = self.try_get_log_entries(start..end).await?;

            for ent in entries.iter().rev() {
                if let EntryPayload::Membership(ref mem) = ent.payload {
                    return Ok(Some(EffectiveMembership {
                        log_id: ent.log_id,
                        membership: mem.clone(),
                    }));
                }
            }

            end = end.saturating_sub(step);
        }

        Ok(None)
    }

    /// Returns the first log id in log.
    ///
    /// The impl should not consider the applied log id in state machine.
    async fn first_id_in_log(&self) -> Result<Option<LogId>, StorageError> {
        let (first_log_id, _) = self.get_log_state().await?;
        Ok(first_log_id)
    }

    /// Returns the last log id in log.
    ///
    /// The impl should not consider the applied log id in state machine.
    async fn last_id_in_log(&self) -> Result<Option<LogId>, StorageError> {
        let (_, last_log_id) = self.get_log_state().await?;
        Ok(last_log_id)
    }

    /// Returns first known log id in logs or in state machine.
    ///
    /// It returns None only when there is never a log.
    async fn first_known_log_id(&self) -> Result<Option<LogId>, StorageError> {
        let (last_applied, _) = self.last_applied_state().await?;
        let (first, _) = self.get_log_state().await?;

        if last_applied.is_none() {
            return Ok(first);
        }

        if first.is_none() {
            return Ok(last_applied);
        }

        Ok(std::cmp::min(first, last_applied))
    }

    /// Get Raft's state information from storage.
    ///
    /// When the Raft node is first started, it will call this interface to fetch the last known state from stable
    /// storage.
    async fn get_initial_state(&self) -> Result<InitialState, StorageError> {
        let hs = self.read_hard_state().await?;

        // Search for two place and use the max one,
        // because when a state machine is installed there could be logs
        // included in the state machine that are not cleaned:
        // - the last log id
        // - the last_applied log id in state machine.

        let (last_applied, _) = self.last_applied_state().await?;
        let last_id_in_log = self.last_id_in_log().await?;
        let last_log_id = std::cmp::max(last_applied, last_id_in_log);

        let membership = self.get_membership().await?;

        Ok(InitialState {
            last_log_id,
            last_applied,
            hard_state: hs.unwrap_or_default(),
            last_membership: membership,
        })
    }

    /// Get a series of log entries from storage.
    ///
    /// Similar to `try_get_log_entries` except an error will be returned if there is an entry not found in the
    /// specified range.
    async fn get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RB,
    ) -> Result<Vec<Entry<D>>, StorageError> {
        // TODO(xp): test: expect an error if a specified entry is not found
        let res = self.try_get_log_entries(range.clone()).await?;

        check_range_matches_entries(range, &res)?;

        Ok(res)
    }

    /// Try to get an log entry.
    ///
    /// It does not return an error if the log entry at `log_index` is not found.
    async fn try_get_log_entry(&self, log_index: u64) -> Result<Option<Entry<D>>, StorageError> {
        let mut res = self.try_get_log_entries(log_index..(log_index + 1)).await?;
        Ok(res.pop())
    }

    // --- Hard State

    async fn save_hard_state(&self, hs: &HardState) -> Result<(), StorageError>;

    async fn read_hard_state(&self) -> Result<Option<HardState>, StorageError>;

    // --- Log

    /// Returns the fist log id and last log id in log.
    ///
    /// The impl should not consider the applied log id in state machine.
    async fn get_log_state(&self) -> Result<(Option<LogId>, Option<LogId>), StorageError>;

    /// Get a series of log entries from storage.
    ///
    /// The start value is inclusive in the search and the stop value is non-inclusive: `[start, stop)`.
    ///
    /// Entry that is not found is allowed.
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RB,
    ) -> Result<Vec<Entry<D>>, StorageError>;

    /// Append a payload of entries to the log.
    ///
    /// Though the entries will always be presented in order, each entry's index should be used to
    /// determine its location to be written in the log.
    async fn append_to_log(&self, entries: &[&Entry<D>]) -> Result<(), StorageError>;

    /// Delete all logs in a `range`.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn delete_log<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RB,
    ) -> Result<(), StorageError>;

    // --- State Machine

    /// Returns the last applied log id which is recorded in state machine, and the last applied membership log id and
    /// membership config.
    async fn last_applied_state(&self) -> Result<(Option<LogId>, Option<EffectiveMembership>), StorageError>;

    /// Apply the given payload of entries to the state machine.
    ///
    /// The Raft protocol guarantees that only logs which have been _committed_, that is, logs which
    /// have been replicated to a quorum of the cluster, will be applied to the state machine.
    ///
    /// This is where the business logic of interacting with your application's state machine
    /// should live. This is 100% application specific. Perhaps this is where an application
    /// specific transaction is being started, or perhaps committed. This may be where a key/value
    /// is being stored.
    ///
    /// An impl should do:
    /// - Store the last applied log id.
    /// - Deal with the EntryPayload::Normal() log, which is business logic log.
    /// - Deal with EntryPayload::Membership, store the membership config.
    async fn apply_to_state_machine(&self, entries: &[&Entry<D>]) -> Result<Vec<R>, StorageError>;

    // --- Snapshot

    /// Build snapshot
    ///
    /// A snapshot has to contain information about exactly all logs upto the last applied.
    ///
    /// Building snapshot can be done by:
    /// - Performing log compaction, e.g. merge log entries that operates on the same key, like a LSM-tree does,
    /// - or by fetching a snapshot from the state machine.
    async fn build_snapshot(&self) -> Result<Snapshot<Self::SnapshotData>, StorageError>;

    /// Create a new blank snapshot, returning a writable handle to the snapshot object.
    ///
    /// Raft will use this handle to receive snapshot data.
    ///
    /// ### implementation guide
    /// See the [storage chapter of the guide](https://datafuselabs.github.io/openraft/storage.html)
    /// for details on log compaction / snapshotting.
    async fn begin_receiving_snapshot(&self) -> Result<Box<Self::SnapshotData>, StorageError>;

    /// Finalize the installation of a snapshot which has finished streaming from the cluster leader.
    ///
    /// All other snapshots should be deleted at this point.
    ///
    /// ### snapshot
    /// A snapshot created from an earlier call to `begin_receiving_snapshot` which provided the snapshot.
    async fn finalize_snapshot_installation(
        &self,
        meta: &SnapshotMeta,
        snapshot: Box<Self::SnapshotData>,
    ) -> Result<StateMachineChanges, StorageError>;

    /// Get a readable handle to the current snapshot, along with its metadata.
    ///
    /// ### implementation algorithm
    /// Implementing this method should be straightforward. Check the configured snapshot
    /// directory for any snapshot files. A proper implementation will only ever have one
    /// active snapshot, though another may exist while it is being created. As such, it is
    /// recommended to use a file naming pattern which will allow for easily distinguishing between
    /// the current live snapshot, and any new snapshot which is being created.
    ///
    /// A proper snapshot implementation will store the term, index and membership config as part
    /// of the snapshot, which should be decoded for creating this method's response data.
    async fn get_current_snapshot(&self) -> Result<Option<Snapshot<Self::SnapshotData>>, StorageError>;
}

/// APIs for debugging a store.
#[async_trait]
pub trait RaftStorageDebug<SM> {
    /// Get a handle to the state machine for testing purposes.
    async fn get_state_machine(&self) -> SM;
}
