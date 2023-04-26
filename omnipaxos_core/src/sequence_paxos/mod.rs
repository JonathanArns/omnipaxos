use super::{
    ballot_leader_election::Ballot,
    messages::sequence_paxos::*,
    storage::{Entry, Snapshot, StopSign, StopSignEntry, Storage},
    util::{defaults::BUFFER_SIZE, LeaderState},
};
#[cfg(feature = "logging")]
use crate::utils::logger::create_logger;
use crate::{
    omni_paxos::{CompactionErr, OmniPaxosConfig, ProposeErr, ReconfigurationRequest},
    storage::InternalStorage,
    util::{ConfigurationId, NodeId},
};
#[cfg(feature = "logging")]
use slog::{debug, info, trace, Logger};
use std::{fmt::Debug, marker::PhantomData, vec, future::Future};
#[cfg(feature = "async")]
use std::sync::Arc;
#[cfg(feature = "async")]
use tokio::sync::broadcast;

pub mod follower;
pub mod leader;

/// Contains metadata about entries that is used to resolve
/// or cancel waiting futures in the async API.
#[cfg(feature = "async")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShadowEntry {
    /// The proposer's PID
    pub(crate) proposer: NodeId,
    /// a proposal counter, locally unique to the proposer
    pub(crate) counter: u64,
    /// The proposer's promised ballot at the time of proposal
    pub(crate) ballot: Ballot,
    /// indicates if this entry was proposed by the async API or not
    pub(crate) has_future: bool,
}

impl ShadowEntry {
    fn generate(ballot: Ballot, has_future: bool) -> Self {
        todo!()
    }
}

/// a Sequence Paxos replica. Maintains local state of the replicated log, handles incoming messages and produces outgoing messages that the user has to fetch periodically and send using a network implementation.
/// User also has to periodically fetch the decided entries that are guaranteed to be strongly consistent and linearizable, and therefore also safe to be used in the higher level application.
/// If snapshots are not desired to be used, use `()` for the type parameter `S`.
pub(crate) struct SequencePaxos<T, S, B, C>
where
    T: Entry,
    S: Snapshot<T>,
    B: Storage<T, S>,
    C: Storage<Option<ShadowEntry>, ()>,
{
    pub(crate) internal_storage: InternalStorage<B, T, S>,
    config_id: ConfigurationId,
    pid: NodeId,
    peers: Vec<u64>, // excluding self pid
    state: (Role, Phase),
    leader: Ballot,
    pending_proposals: Vec<T>,
    pending_shadow_proposals: Vec<Option<ShadowEntry>>,
    pending_stopsign: Option<StopSign>,
    outgoing: Vec<PaxosMessage<T, S>>,
    leader_state: LeaderState<T, S>,
    latest_accepted_meta: Option<(Ballot, usize)>,
    buffer_size: usize,
    s: PhantomData<S>,
    #[cfg(feature = "logging")]
    logger: Logger,
    // #[cfg(feature = "async")]
    // send_msg_callback: dyn Fn(PaxosMessage<T, S>) -> dyn Future<Output = ()>,
    #[cfg(feature = "async")]
    shadow_log: InternalStorage<C, Option<ShadowEntry>, ()>,
    #[cfg(feature = "async")]
    shadow_broadcast: Arc<broadcast::Sender<ShadowEntry>>,
}

impl<T, S, B, C> SequencePaxos<T, S, B, C>
where
    T: Entry,
    S: Snapshot<T>,
    B: Storage<T, S>,
    C: Storage<Option<ShadowEntry>, ()>,
{
    /*** User functions ***/
    /// Creates a Sequence Paxos replica.
    pub(crate) fn with(config: SequencePaxosConfig, storage: B, shadow_storage: C) -> Self {
        let pid = config.pid;
        let peers = config.peers;
        let config_id = config.configuration_id;
        let num_nodes = &peers.len() + 1;
        let majority = num_nodes / 2 + 1;
        let max_peer_pid = peers.iter().max().unwrap();
        let max_pid = *std::cmp::max(max_peer_pid, &pid) as usize;
        let (state, leader, lds) = match &config.skip_prepare_use_leader {
            Some(l) => {
                let (role, lds) = if l.pid == pid {
                    // we are leader in new config
                    let mut v = vec![None; max_pid];
                    for idx in peers.iter().map(|pid| *pid as usize - 1) {
                        // this works as a promise
                        v[idx] = Some(0);
                    }
                    (Role::Leader, Some(v))
                } else {
                    (Role::Follower, None)
                };
                let state = (role, Phase::FirstAccept);
                (state, *l, lds)
            }
            None => {
                let state = (Role::Follower, Phase::None);
                let lds = None;
                (state, Ballot::default(), lds)
            }
        };
        
        let (bcast, _) = broadcast::channel(1000); // TODO: pick better buffer size

        let internal_storage = InternalStorage::with(storage);
        let shadow_log = InternalStorage::with(shadow_storage);
        let mut paxos = SequencePaxos {
            shadow_log,
            internal_storage,
            config_id,
            pid,
            peers,
            state,
            pending_proposals: vec![],
            pending_shadow_proposals: vec![],
            pending_stopsign: None,
            leader,
            outgoing: Vec::with_capacity(BUFFER_SIZE),
            leader_state: LeaderState::<T, S>::with(leader, lds, max_pid, majority),
            latest_accepted_meta: None,
            buffer_size: config.buffer_size,
            s: PhantomData,
            #[cfg(feature = "logging")]
            logger: {
                let s = config
                    .logger_file_path
                    .unwrap_or_else(|| format!("logs/paxos_{}.log", pid));
                create_logger(s.as_str())
            },
            #[cfg(feature = "async")]
            shadow_broadcast: Arc::new(bcast),
        };
        paxos.internal_storage.set_promise(leader);
        #[cfg(feature = "logging")]
        {
            info!(paxos.logger, "Paxos component pid: {} created!", pid);
        }
        paxos
    }

    /// Initiates the trim process.
    /// # Arguments
    /// * `trim_idx` - Deletes all entries up to [`trim_idx`], if the [`trim_idx`] is `None` then the minimum index accepted by **ALL** servers will be used as the [`trim_idx`].
    pub(crate) fn trim(&mut self, trim_idx: Option<u64>) -> Result<(), CompactionErr> {
        match self.state {
            (Role::Leader, _) => {
                let min_all_accepted_idx = self.leader_state.get_min_all_accepted_idx();
                let trimmed_idx = match trim_idx {
                    Some(idx) if idx <= *min_all_accepted_idx => idx,
                    None => {
                        #[cfg(feature = "logging")]
                        trace!(
                            self.logger,
                            "No trim index provided, using min_las_idx: {:?}",
                            min_all_accepted_idx
                        );
                        *min_all_accepted_idx
                    }
                    _ => {
                        return Err(CompactionErr::NotAllDecided(*min_all_accepted_idx));
                    }
                };
                let result = self.internal_storage.try_trim(trimmed_idx);
                if result.is_ok() {
                    for pid in self.peers.clone() {
                        let msg = PaxosMsg::Compaction(Compaction::Trim(trimmed_idx));
                        self.send_msg(PaxosMessage {
                            from: self.pid,
                            to: pid,
                            msg,
                        });
                    }
                }
                result
            }
            _ => Err(CompactionErr::NotCurrentLeader(self.leader.pid)),
        }
    }

    /// Trim the log and create a snapshot. ** Note: only up to the `decided_idx` can be snapshotted **
    /// # Arguments
    /// `idx` - Snapshots all entries with index < [`idx`], if the [`idx`] is None then the decided index will be used.
    /// `local_only` - If `true`, only this server snapshots the log. If `false` all servers performs the snapshot.
    pub(crate) fn snapshot(
        &mut self,
        idx: Option<u64>,
        local_only: bool,
    ) -> Result<(), CompactionErr> {
        let result = self.internal_storage.try_snapshot(idx);
        if !local_only && result.is_ok() {
            // since it is decided, it is ok even for a follower to send this
            for pid in self.peers.clone() {
                let msg = PaxosMsg::Compaction(Compaction::Snapshot(idx));
                self.send_msg(PaxosMessage {
                    from: self.pid,
                    to: pid,
                    msg,
                });
            }
        }
        result
    }

    /// Return the decided index.
    pub(crate) fn get_decided_idx(&self) -> u64 {
        self.internal_storage.get_decided_idx()
    }

    /// Return trim index from storage.
    pub(crate) fn get_compacted_idx(&self) -> u64 {
        self.internal_storage.get_compacted_idx()
    }

    /// Recover from failure. Goes into recover state and sends `PrepareReq` to all peers.
    pub(crate) fn fail_recovery(&mut self) {
        self.state = (Role::Follower, Phase::Recover);
        for pid in self.peers.clone() {
            self.send_msg(PaxosMessage {
                from: self.pid,
                to: pid,
                msg: PaxosMsg::PrepareReq,
            });
        }
    }

    fn handle_compaction(&mut self, c: Compaction) {
        // try trimming and snapshotting forwarded compaction. Errors are ignored as that the data will still be kept.
        match c {
            Compaction::Trim(idx) => {
                let _ = self.internal_storage.try_trim(idx);
            }
            Compaction::Snapshot(idx) => {
                let _ = self.snapshot(idx, true);
            }
        }
    }

    /// Constructs a shadow_log suffix, prefixed by [None] if some entries are not available.
    fn construct_shadow_log_suffix(&self, idx: usize) -> Vec<Option<ShadowEntry>> {
        let compacted_idx = self.shadow_log.get_compacted_idx();
        if compacted_idx > idx {
            let suffix = vec![None; compacted_idx - idx];
            suffix.append(&mut self.shadow_log.get_suffix(compacted_idx));
            suffix
        } else {
            self.shadow_log.get_suffix(idx)
        }
    }

    /// Returns the id of the current leader.
    pub(crate) fn get_current_leader(&self) -> Ballot {
        self.leader
    }

    /// Returns the outgoing messages from this replica. The messages should then be sent via the network implementation.
    pub(crate) fn get_outgoing_msgs(&mut self) -> Vec<PaxosMessage<T, S>> {
        let mut outgoing = Vec::with_capacity(self.buffer_size);
        std::mem::swap(&mut self.outgoing, &mut outgoing);
        #[cfg(feature = "batch_accept")]
        {
            self.leader_state.reset_batch_accept_meta();
        }
        self.latest_accepted_meta = None;
        outgoing
    }

    fn send_msg(&mut self, msg: PaxosMessage<T, S>) {
        // #[cfg(feature = "async")]
        // {
        //     todo!()
        // }
        // #[cfg(not(feature = "async"))]
        self.outgoing.push(msg);
    }

    /// Handle an incoming message.
    pub(crate) fn handle(&mut self, m: PaxosMessage<T, S>) {
        match m.msg {
            PaxosMsg::PrepareReq => self.handle_preparereq(m.from),
            PaxosMsg::Prepare(prep) => self.handle_prepare(prep, m.from),
            PaxosMsg::Promise(prom) => match &self.state {
                (Role::Leader, Phase::Prepare) => self.handle_promise_prepare(prom, m.from),
                (Role::Leader, Phase::Accept) => self.handle_promise_accept(prom, m.from),
                _ => {}
            },
            PaxosMsg::AcceptSync(acc_sync) => self.handle_acceptsync(acc_sync, m.from),
            PaxosMsg::FirstAccept(f) => self.handle_firstaccept(f),
            PaxosMsg::AcceptDecide(acc) => self.handle_acceptdecide(acc),
            PaxosMsg::Accepted(accepted) => self.handle_accepted(accepted, m.from),
            PaxosMsg::Decide(d) => self.handle_decide(d),
            PaxosMsg::ProposalForward(entries, shadow_entries) => self.handle_forwarded_proposal(entries, shadow_entries),
            PaxosMsg::Compaction(c) => self.handle_compaction(c),
            PaxosMsg::AcceptStopSign(acc_ss) => self.handle_accept_stopsign(acc_ss),
            PaxosMsg::AcceptedStopSign(acc_ss) => self.handle_accepted_stopsign(acc_ss, m.from),
            PaxosMsg::DecideStopSign(d_ss) => self.handle_decide_stopsign(d_ss),
            PaxosMsg::ForwardStopSign(f_ss) => self.handle_forwarded_stopsign(f_ss),
        }
    }

    /// Returns whether this Sequence Paxos has been reconfigured
    pub(crate) fn is_reconfigured(&self) -> Option<StopSign> {
        match self.internal_storage.get_stopsign() {
            Some(ss) if ss.decided => Some(ss.stopsign),
            _ => None,
        }
    }

    /// Returns whether this Sequence Paxos instance is stopped, i.e. if it has been reconfigured.
    fn stopped(&self) -> bool {
        self.get_stopsign().is_some()
    }

    /// Append an entry to the replicated log.
    pub(crate) fn append(&mut self, entry: T) -> Result<(), ProposeErr<T>> {
        if self.stopped() {
            Err(ProposeErr::Normal(entry))
        } else {
            let shadow_entry = ShadowEntry::generate(self.internal_storage.get_promise(), false);
            self.propose_entry(entry, shadow_entry);
            Ok(())
        }
    }

    /// Append an entry to the replicated log.
    /// The returned future resolves when the appended entry is decided.
    #[cfg(feature = "async")]
    pub(crate) async fn append_async(&mut self, entry: T) -> Result<(), ProposeErr<T>> {
        if self.stopped() {
            return Err(ProposeErr::Normal(entry))
        }
        let shadow_entry = ShadowEntry::generate(self.internal_storage.get_promise(), true);

        // we need to start listening before the actual proposal, so we cannot miss the decision
        let mut receiver = self.shadow_broadcast.clone().subscribe();
        self.propose_entry(entry, shadow_entry);
        loop {
            if let Ok(decided) = receiver.recv().await {
                todo!("resolve or cancel logic")
            }
        }
    }

    #[cfg(feature = "async")]
    fn notify_on_decided(&mut self, entry_id: ShadowEntry) {
        let sender = self.shadow_broadcast.clone();
        sender.send(entry_id);
    }

    fn update_decided_idx(&mut self, decided_idx: u64) {
        self.internal_storage.set_decided_idx(decided_idx);
        #[cfg(feature = "async")]
        {
            let range = 0..(decided_idx-self.shadow_log.offset) as usize;
            for i in range.clone() {
                if let Some(id) = self.shadow_log.log[i] {
                    self.notify_on_decided(id);
                }
            }
            self.shadow_log.log.drain(range);
            self.shadow_log.offset = self.shadow_log.offset.max(decided_idx);
        }
    }

    /// Propose a reconfiguration. Returns error if already stopped or new configuration is empty.
    pub(crate) fn reconfigure(&mut self, rc: ReconfigurationRequest) -> Result<(), ProposeErr<T>> {
        let ReconfigurationRequest {
            new_configuration,
            metadata,
        } = rc;
        #[cfg(feature = "logging")]
        info!(
            self.logger,
            "Propose reconfiguration {:?}", new_configuration
        );
        if self.stopped() {
            Err(ProposeErr::Reconfiguration(new_configuration))
        } else {
            match self.state {
                (Role::Leader, Phase::Prepare) => {
                    if self.pending_stopsign.is_none() {
                        let ss = StopSign::with(self.config_id + 1, new_configuration, metadata);
                        self.pending_stopsign = Some(ss);
                    } else {
                        return Err(ProposeErr::Reconfiguration(new_configuration));
                    }
                }
                (Role::Leader, Phase::Accept) => {
                    if !self.stopped() {
                        let ss = StopSign::with(self.config_id + 1, new_configuration, metadata);
                        self.accept_stopsign(ss.clone());
                        self.send_accept_stopsign(ss);
                    } else {
                        return Err(ProposeErr::Reconfiguration(new_configuration));
                    }
                }
                (Role::Leader, Phase::FirstAccept) => {
                    if !self.stopped() {
                        self.send_first_accept();
                        let ss = StopSign::with(self.config_id + 1, new_configuration, metadata);
                        self.accept_stopsign(ss.clone());
                        self.send_accept_stopsign(ss);
                    } else {
                        return Err(ProposeErr::Reconfiguration(new_configuration));
                    }
                }
                _ => {
                    let ss = StopSign::with(self.config_id + 1, new_configuration, metadata);
                    self.forward_stopsign(ss);
                }
            }
            Ok(())
        }
    }

    fn send_accept_stopsign(&mut self, ss: StopSign) {
        let acc_ss = PaxosMsg::AcceptStopSign(AcceptStopSign {
            n: self.leader_state.n_leader,
            ss,
        });
        for pid in self.leader_state.get_promised_followers() {
            self.send_msg(PaxosMessage {
                from: self.pid,
                to: pid,
                msg: acc_ss.clone(),
            });
        }
    }

    fn accept_stopsign(&mut self, ss: StopSign) {
        self.internal_storage
            .set_stopsign(StopSignEntry::with(ss, false));
        if self.state.0 == Role::Leader {
            self.leader_state.set_accepted_stopsign(self.pid);
        }
    }

    /// Handles re-establishing a connection to a previously disconnected peer.
    /// This should only be called if the underlying network implementation indicates that a connection has been re-established.
    pub(crate) fn reconnected(&mut self, pid: NodeId) {
        if pid == self.pid {
            return;
        } else if pid == self.leader.pid {
            self.state = (Role::Follower, Phase::Recover);
        }
        self.send_msg(PaxosMessage {
            from: self.pid,
            to: pid,
            msg: PaxosMsg::PrepareReq,
        });
    }

    fn propose_entry(&mut self, entry: T, shadow_entry: ShadowEntry) {
        match self.state {
            (Role::Leader, Phase::Prepare) => self.pending_proposals.push(entry),
            (Role::Leader, Phase::Accept) => self.send_accept(entry, Some(shadow_entry)),
            (Role::Leader, Phase::FirstAccept) => {
                self.send_first_accept();
                self.send_accept(entry, Some(shadow_entry));
            }
            _ => self.forward_proposals(vec![entry], vec![Some(shadow_entry)]),
        }
        todo!("use shadow entry")
    }

    fn get_stopsign(&self) -> Option<StopSign> {
        self.internal_storage.get_stopsign().map(|x| x.stopsign)
    }

    pub(crate) fn use_snapshots() -> bool {
        S::use_snapshots()
    }
}

#[derive(PartialEq, Debug)]
enum Phase {
    Prepare,
    FirstAccept,
    Accept,
    Recover,
    None,
}

#[derive(PartialEq, Debug)]
enum Role {
    Follower,
    Leader,
}

/// Configuration for `SequencePaxos`.
/// # Fields
/// * `configuration_id`: The identifier for the configuration that this Sequence Paxos replica is part of.
/// * `pid`: The unique identifier of this node. Must not be 0.
/// * `peers`: The peers of this node i.e. the `pid`s of the other replicas in the configuration.
/// * `buffer_size`: The buffer size for outgoing messages.
/// * `skip_prepare_use_leader`: The initial leader of the cluster. Could be used in combination with reconfiguration to skip the prepare phase in the new configuration.
/// * `logger`: Custom logger for logging events of Sequence Paxos.
/// * `logger_file_path`: The path where the default logger logs events.
#[derive(Clone, Debug)]
pub struct SequencePaxosConfig {
    configuration_id: u32,
    pid: NodeId,
    peers: Vec<u64>,
    buffer_size: usize,
    skip_prepare_use_leader: Option<Ballot>,
    #[cfg(feature = "logging")]
    logger_file_path: Option<String>,
}

impl From<OmniPaxosConfig> for SequencePaxosConfig {
    fn from(config: OmniPaxosConfig) -> Self {
        SequencePaxosConfig {
            configuration_id: config.configuration_id,
            pid: config.pid,
            peers: config.peers,
            buffer_size: config.buffer_size,
            skip_prepare_use_leader: config.skip_prepare_use_leader,
            #[cfg(feature = "logging")]
            logger_file_path: config.logger_file_path,
        }
    }
}
