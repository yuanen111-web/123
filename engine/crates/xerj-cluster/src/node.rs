//! Cluster node — wraps [`RaftNode`] + [`ClusterMetadata`] + transport.
//!
//! [`ClusterNode`] is the top-level runtime object that ties together:
//! - the pure Raft state machine ([`RaftNode`]),
//! - the replicated metadata store ([`ClusterMetadata`]),
//! - and an I/O transport ([`ClusterTransport`]) for sending/receiving messages.
//!
//! In production, the transport will be a gRPC/QUIC channel. In tests, an
//! in-memory channel is used (see [`transport`] module).

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::metadata::ClusterMetadata;
use crate::raft::{ClusterCommand, LogEntry, OutboundMessage, RaftMessage, RaftNode, RaftState};

// ── Transport abstraction ─────────────────────────────────────────────────────

/// Asynchronous message transport between cluster nodes.
///
/// Implementors are responsible for serialisation and delivery. The Raft state
/// machine itself is purely synchronous — the transport bridges it to async I/O.
#[async_trait]
pub trait ClusterTransport: Send + Sync {
    /// Send a Raft message to the node identified by `to`.
    async fn send(&self, to: &str, msg: RaftMessage) -> Result<()>;

    /// Receive the next inbound message. Returns `(sender_id, message)`.
    async fn recv(&self) -> Result<(String, RaftMessage)>;
}

// ── Engine / State-Machine Applier & Handle ──────────────────────────────────

/// Callback hook invoked by [`ClusterNode`] whenever a [`LogEntry`] is committed
/// by Raft and applied to [`ClusterMetadata`].
///
/// The storage engine implements this trait so that committed index DDL
/// (`CreateIndex`, `DeleteIndex`, `UpdateMapping`) and document DML
/// (`IndexDocument`, `BulkIndexDocuments`, `DeleteDocument`) replicate onto
/// every node's local [`Engine`] instance.
#[async_trait]
pub trait ClusterApplier: Send + Sync {
    /// Apply a newly committed [`LogEntry`] to the local storage engine.
    async fn apply_committed(&self, entry: &LogEntry, metadata: &ClusterMetadata);

    /// Synchronise any cluster-wide metadata snapshots after applying an entry.
    fn sync_metadata(&self, _metadata: &ClusterMetadata) {}
}

/// Thread-safe snapshot of a node's live Raft state and replicated [`ClusterMetadata`].
#[derive(Debug, Clone)]
pub struct ClusterStatusSnapshot {
    pub node_id: String,
    pub state: RaftState,
    pub current_term: u64,
    pub leader_id: Option<String>,
    pub commit_index: u64,
    pub last_applied: u64,
    pub peers: Vec<String>,
    pub metadata: ClusterMetadata,
}

struct ClusterProposalRequest {
    command: ClusterCommand,
    /// When `true`, the local engine already applied the mutation before
    /// submitting the Raft command, so `apply_committed` is skipped once on
    /// this node when the command commits.
    preapplied: bool,
    commit_tx: Option<oneshot::Sender<Result<u64>>>,
}

/// Cloneable handle to a running [`ClusterNode`].
///
/// Allows the storage engine and HTTP handlers to read live Raft status
/// (`leader_id`, `state`, `current_term`, `commit_index`, `metadata`) and
/// submit [`ClusterCommand`] proposals from both synchronous and asynchronous
/// call sites without holding a mutable borrow of [`ClusterNode`].
#[derive(Clone)]
pub struct ClusterHandle {
    status: Arc<RwLock<ClusterStatusSnapshot>>,
    cmd_tx: mpsc::UnboundedSender<ClusterProposalRequest>,
}

impl std::fmt::Debug for ClusterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snap = self.status_snapshot();
        f.debug_struct("ClusterHandle")
            .field("node_id", &snap.node_id)
            .field("state", &snap.state)
            .field("current_term", &snap.current_term)
            .field("leader_id", &snap.leader_id)
            .field("commit_index", &snap.commit_index)
            .finish()
    }
}

impl ClusterHandle {
    /// Read the most recent [`ClusterStatusSnapshot`] published by the [`ClusterNode`].
    pub fn status_snapshot(&self) -> ClusterStatusSnapshot {
        self.status
            .read()
            .expect("ClusterStatusSnapshot RwLock poisoned")
            .clone()
    }

    /// Enqueue a [`ClusterCommand`] whose local side-effects have already been
    /// applied on this node. Non-blocking; safe to call from synchronous code.
    pub fn submit_preapplied(&self, command: ClusterCommand) {
        let _ = self.cmd_tx.send(ClusterProposalRequest {
            command,
            preapplied: true,
            commit_tx: None,
        });
    }

    /// Enqueue a [`ClusterCommand`] whose local side-effects have already been
    /// applied on this node, and wait up to `timeout` for Raft commit.
    ///
    /// If the cluster has no active runner or cannot reach quorum within
    /// `timeout` (for example in a degraded partition), returns `Err` after
    /// `timeout` while the proposal remains queued in Raft.
    pub async fn replicate_preapplied(
        &self,
        command: ClusterCommand,
        timeout: Duration,
    ) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ClusterProposalRequest {
                command,
                preapplied: true,
                commit_tx: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("cluster node channel closed"))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(anyhow::anyhow!("cluster proposal waiter dropped")),
            Err(_) => Err(anyhow::anyhow!("timed out waiting for Raft commit")),
        }
    }

    /// Submit a [`ClusterCommand`] through Raft without pre-applying locally
    /// (so all nodes, including the proposer, invoke [`ClusterApplier::apply_committed`]
    /// once committed) and wait up to `timeout` for commit.
    pub async fn propose(&self, command: ClusterCommand, timeout: Duration) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ClusterProposalRequest {
                command,
                preapplied: false,
                commit_tx: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("cluster node channel closed"))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(anyhow::anyhow!("cluster proposal waiter dropped")),
            Err(_) => Err(anyhow::anyhow!("timed out waiting for Raft commit")),
        }
    }
}

// ── Cluster node ──────────────────────────────────────────────────────────────

/// A running cluster node.
///
/// Drives the Raft state machine forward via periodic ticks and delivers
/// committed log entries to the local [`ClusterMetadata`] state machine and
/// optional [`ClusterApplier`].
pub struct ClusterNode {
    pub raft: RaftNode,
    pub metadata: ClusterMetadata,
    transport: Box<dyn ClusterTransport>,
    status: Arc<RwLock<ClusterStatusSnapshot>>,
    cmd_tx: mpsc::UnboundedSender<ClusterProposalRequest>,
    cmd_rx: mpsc::UnboundedReceiver<ClusterProposalRequest>,
    applier: Option<Arc<dyn ClusterApplier>>,
    preapplied_commands: VecDeque<(ClusterCommand, Option<oneshot::Sender<Result<u64>>>)>,
    commit_waiters: VecDeque<(ClusterCommand, oneshot::Sender<Result<u64>>)>,
    pending_proposals: VecDeque<ClusterProposalRequest>,
}

impl ClusterNode {
    /// Create a new cluster node with the given identity, peers, and transport.
    pub fn new(id: String, peers: Vec<String>, transport: Box<dyn ClusterTransport>) -> Self {
        let raft = RaftNode::new(id, peers);
        let metadata = ClusterMetadata::new();
        let status = Arc::new(RwLock::new(ClusterStatusSnapshot {
            node_id: raft.id.clone(),
            state: raft.state().clone(),
            current_term: raft.current_term(),
            leader_id: raft.leader_id().map(|s| s.to_string()),
            commit_index: raft.commit_index(),
            last_applied: raft.last_applied(),
            peers: raft.peers().to_vec(),
            metadata: metadata.clone(),
        }));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        ClusterNode {
            raft,
            metadata,
            transport,
            status,
            cmd_tx,
            cmd_rx,
            applier: None,
            preapplied_commands: VecDeque::new(),
            commit_waiters: VecDeque::new(),
            pending_proposals: VecDeque::new(),
        }
    }

    /// Attach a [`ClusterApplier`] (such as [`xerj_engine::Engine`]) that will
    /// receive every committed [`LogEntry`].
    pub fn set_applier(&mut self, applier: Arc<dyn ClusterApplier>) {
        self.applier = Some(applier);
    }

    /// Return a cloneable [`ClusterHandle`] connected to this node.
    pub fn handle(&self) -> ClusterHandle {
        ClusterHandle {
            status: Arc::clone(&self.status),
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Tick the Raft state machine and dispatch any outbound messages.
    ///
    /// Call this periodically (e.g. every 10 ms) from the cluster run loop.
    pub async fn tick(&mut self) -> Result<()> {
        let msgs = self.raft.tick();
        self.dispatch(msgs).await?;
        self.drain_pending_proposals().await?;
        self.apply_ready().await;
        Ok(())
    }

    /// Process a single inbound message from a peer.
    pub async fn handle_message(&mut self, from: &str, msg: RaftMessage) -> Result<()> {
        debug!(node = %self.raft.id, %from, "Handling inbound Raft message");
        let _ = from; // sender is embedded in message variants that need it
        let prev_commit = self.raft.commit_index();
        let replies = self.raft.handle_message(msg);
        self.dispatch(replies).await?;
        if self.raft.is_leader() && self.raft.commit_index() > prev_commit {
            let commit_hb = self.raft.force_heartbeat();
            self.dispatch(commit_hb).await?;
        }
        self.drain_pending_proposals().await?;
        self.apply_ready().await;
        Ok(())
    }

    /// Propose a command through Raft consensus (leader only).
    ///
    /// After calling this, the entry will be replicated and, once committed,
    /// automatically applied to the local [`ClusterMetadata`] via the run loop.
    pub fn propose(&mut self, cmd: ClusterCommand) -> Result<u64> {
        let res = self.raft.propose(cmd);
        self.update_status();
        res
    }

    /// Perform one iteration of the node event loop:
    /// 1. Drain any ready outbound proposals from [`ClusterHandle`].
    /// 2. Wait for an inbound peer message or a new proposal, bounded by `tick_interval`.
    /// 3. Tick the state machine (advancing elections / heartbeats).
    pub async fn step(&mut self, tick_interval: Duration) -> Result<()> {
        self.drain_pending_proposals().await?;

        tokio::select! {
            recv_result = tokio::time::timeout(tick_interval, self.transport.recv()) => {
                match recv_result {
                    Ok(Ok((from, msg))) => {
                        self.handle_message(&from, msg).await?;
                    }
                    Ok(Err(e)) => {
                        warn!(node = %self.raft.id, error = %e, "Transport recv error");
                    }
                    Err(_timeout) => {
                        // Timeout is normal — proceed to tick.
                    }
                }
            }
            Some(req) = self.cmd_rx.recv() => {
                self.pending_proposals.push_back(req);
                self.drain_pending_proposals().await?;
            }
        }

        self.tick().await?;
        Ok(())
    }

    /// Run the node event loop forever (until an error is hit).
    pub async fn run(&mut self, tick_interval: Duration) -> Result<()> {
        info!(node = %self.raft.id, "Starting cluster node run loop");
        loop {
            self.step(tick_interval).await?;
        }
    }

    /// Whether this node is the current Raft leader.
    pub fn is_leader(&self) -> bool {
        self.raft.is_leader()
    }

    /// The node ID of the current Raft leader, if known.
    pub fn leader_id(&self) -> Option<&str> {
        self.raft.leader_id()
    }

    /// Current Raft state.
    pub fn state(&self) -> &RaftState {
        self.raft.state()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn update_status(&self) {
        if let Ok(mut guard) = self.status.write() {
            *guard = ClusterStatusSnapshot {
                node_id: self.raft.id.clone(),
                state: self.raft.state().clone(),
                current_term: self.raft.current_term(),
                leader_id: self.raft.leader_id().map(|s| s.to_string()),
                commit_index: self.raft.commit_index(),
                last_applied: self.raft.last_applied(),
                peers: self.raft.peers().to_vec(),
                metadata: self.metadata.clone(),
            };
        }
    }

    async fn drain_pending_proposals(&mut self) -> Result<()> {
        while let Ok(req) = self.cmd_rx.try_recv() {
            self.pending_proposals.push_back(req);
        }
        if self.raft.is_leader() || self.raft.leader_id().is_some() {
            while let Some(req) = self.pending_proposals.pop_front() {
                self.process_proposal_request(req).await?;
            }
        }
        Ok(())
    }

    async fn process_proposal_request(&mut self, req: ClusterProposalRequest) -> Result<()> {
        if !self.raft.is_leader() && self.raft.leader_id().is_none() {
            self.pending_proposals.push_back(req);
            return Ok(());
        }

        if req.preapplied {
            self.preapplied_commands
                .push_back((req.command.clone(), req.commit_tx));
        } else if let Some(tx) = req.commit_tx {
            self.commit_waiters.push_back((req.command.clone(), tx));
        }

        if self.raft.is_leader() {
            if let Err(e) = self.raft.propose(req.command) {
                warn!(node = %self.raft.id, error = %e, "Failed to propose command on leader");
            } else {
                let hb = self.raft.force_heartbeat();
                self.dispatch(hb).await?;
                self.apply_ready().await;
            }
        } else if let Some(leader) = self.raft.leader_id().map(|s| s.to_string()) {
            self.dispatch(vec![OutboundMessage {
                to: leader,
                msg: RaftMessage::ForwardProposal {
                    from: self.raft.id.clone(),
                    command: req.command,
                },
            }])
            .await?;
        }
        Ok(())
    }

    async fn dispatch(&self, msgs: Vec<OutboundMessage>) -> Result<()> {
        for out in msgs {
            if let Err(e) = self.transport.send(&out.to, out.msg).await {
                warn!(
                    node = %self.raft.id,
                    to = %out.to,
                    error = %e,
                    "Failed to send Raft message"
                );
                // Don't propagate — Raft handles message loss via retransmit
            }
        }
        Ok(())
    }

    /// Drain and apply all newly committed log entries.
    async fn apply_ready(&mut self) {
        loop {
            let entries = self.raft.ready();
            if entries.is_empty() {
                break;
            }
            for entry in entries {
                debug!(
                    node = %self.raft.id,
                    index = entry.index,
                    term = entry.term,
                    "Applying committed entry"
                );
                self.metadata.apply(entry.index, &entry.command);

                if let ClusterCommand::CreateIndex { ref name, .. } = entry.command {
                    if self.raft.is_leader()
                        && !self
                            .metadata
                            .shard_assignments
                            .contains_key(&(name.clone(), 0))
                        && self
                            .raft
                            .propose(ClusterCommand::AssignShard {
                                index: name.clone(),
                                shard: 0,
                                node_id: self.raft.id.clone(),
                            })
                            .is_ok()
                    {
                        let hb = self.raft.force_heartbeat();
                        let _ = self.dispatch(hb).await;
                    }
                }

                if let Some(pos) = self
                    .preapplied_commands
                    .iter()
                    .position(|(cmd, _)| cmd == &entry.command)
                {
                    let (_, commit_tx) = self.preapplied_commands.remove(pos).unwrap();
                    if let Some(ref applier) = self.applier {
                        applier.sync_metadata(&self.metadata);
                    }
                    if let Some(tx) = commit_tx {
                        let _ = tx.send(Ok(entry.index));
                    }
                } else {
                    if let Some(ref applier) = self.applier {
                        applier.apply_committed(&entry, &self.metadata).await;
                        applier.sync_metadata(&self.metadata);
                    }
                    if let Some(pos) = self
                        .commit_waiters
                        .iter()
                        .position(|(cmd, _)| cmd == &entry.command)
                    {
                        let (_, tx) = self.commit_waiters.remove(pos).unwrap();
                        let _ = tx.send(Ok(entry.index));
                    }
                }
            }
        }
        self.update_status();
    }
}

// ── In-memory transport for tests ────────────────────────────────────────────

/// In-memory transport that routes messages via `tokio::sync::mpsc` channels.
///
/// Used in unit tests to avoid network I/O while still exercising the full
/// Raft message-passing logic.
pub mod in_memory {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    type Sender = mpsc::UnboundedSender<(String, RaftMessage)>;
    type Receiver = mpsc::UnboundedReceiver<(String, RaftMessage)>;

    /// A registry of in-memory channels, one per node.
    #[derive(Clone)]
    pub struct InMemoryBus {
        senders: Arc<Mutex<HashMap<String, Sender>>>,
    }

    impl InMemoryBus {
        pub fn new() -> Self {
            InMemoryBus {
                senders: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        /// Register a node and return its dedicated receiver channel.
        pub async fn register(&self, node_id: &str) -> Receiver {
            let (tx, rx) = mpsc::unbounded_channel();
            self.senders.lock().await.insert(node_id.to_string(), tx);
            rx
        }

        /// Send a message directly (used by [`InMemoryTransport`]).
        pub async fn send_to(&self, from: &str, to: &str, msg: RaftMessage) -> Result<()> {
            let senders = self.senders.lock().await;
            if let Some(tx) = senders.get(to) {
                tx.send((from.to_string(), msg))
                    .map_err(|e| anyhow::anyhow!("channel closed: {}", e))?;
            } else {
                // Node not registered — simulate network partition
                debug!(
                    from,
                    to, "InMemoryBus: target node not found (partitioned?)"
                );
            }
            Ok(())
        }
    }

    impl Default for InMemoryBus {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Transport implementation backed by [`InMemoryBus`].
    pub struct InMemoryTransport {
        pub node_id: String,
        bus: InMemoryBus,
        receiver: Arc<Mutex<Receiver>>,
    }

    impl InMemoryTransport {
        pub async fn new(node_id: String, bus: InMemoryBus) -> Self {
            let receiver = bus.register(&node_id).await;
            InMemoryTransport {
                node_id,
                bus,
                receiver: Arc::new(Mutex::new(receiver)),
            }
        }
    }

    #[async_trait]
    impl ClusterTransport for InMemoryTransport {
        async fn send(&self, to: &str, msg: RaftMessage) -> Result<()> {
            self.bus.send_to(&self.node_id, to, msg).await
        }

        async fn recv(&self) -> Result<(String, RaftMessage)> {
            let mut rx = self.receiver.lock().await;
            rx.recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("channel closed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::in_memory::{InMemoryBus, InMemoryTransport};
    use super::*;
    #[allow(unused_imports)]
    use std::time::Duration;

    async fn make_cluster(ids: &[&str]) -> Vec<ClusterNode> {
        let bus = InMemoryBus::new();
        let id_list: Vec<String> = ids.iter().map(|s| s.to_string()).collect();

        let mut nodes = Vec::new();
        for id in ids {
            let peers: Vec<String> = id_list
                .iter()
                .filter(|p| p.as_str() != *id)
                .cloned()
                .collect();
            let transport = InMemoryTransport::new(id.to_string(), bus.clone()).await;
            let node = ClusterNode::new(id.to_string(), peers, Box::new(transport));
            nodes.push(node);
        }
        nodes
    }

    #[tokio::test]
    async fn test_cluster_node_propose_and_apply() {
        // Single-node cluster: easy path
        let mut nodes = make_cluster(&["n1"]).await;

        // Force election timeout
        nodes[0].raft.force_election_timeout();
        nodes[0].tick().await.unwrap();

        assert!(nodes[0].is_leader(), "single node should become leader");

        nodes[0]
            .propose(ClusterCommand::CreateIndex {
                name: "orders".to_string(),
                schema_json: "{}".to_string(),
            })
            .unwrap();

        // Apply ready entries
        nodes[0].tick().await.unwrap();

        assert!(
            nodes[0].metadata.indices.contains_key("orders"),
            "CreateIndex should be applied to metadata"
        );
    }

    #[tokio::test]
    async fn test_metadata_apply_full_pipeline() {
        let mut nodes = make_cluster(&["n1"]).await;

        nodes[0].raft.force_election_timeout();
        nodes[0].tick().await.unwrap();

        // Apply several commands
        nodes[0]
            .propose(ClusterCommand::AddNode {
                node_id: "n2".to_string(),
                address: "127.0.0.1:9201".to_string(),
            })
            .unwrap();
        nodes[0]
            .propose(ClusterCommand::UpdateConfig {
                key: "replicas".to_string(),
                value: "2".to_string(),
            })
            .unwrap();

        nodes[0].tick().await.unwrap();

        assert!(nodes[0].metadata.nodes.contains_key("n2"));
        assert_eq!(nodes[0].metadata.config["replicas"], "2");
    }

    /// End-to-end proof that [`crate::runner::ClusterRunner::run`] actually
    /// drains and handles inbound messages (not just sleeps and ticks).
    ///
    /// Three real [`ClusterNode`]s are wrapped in `ClusterRunner`s over a shared
    /// [`InMemoryBus`] and driven by their `run()` loops concurrently. A leader
    /// can only emerge if RequestVote / vote-response messages are received and
    /// handled during the run, so asserting exactly one leader within ~2s proves
    /// the run loop processes messages. With the old recv-less `sleep + tick`
    /// body, votes were sent but never received, so zero leaders emerged and
    /// this assertion fails.
    #[tokio::test]
    async fn test_cluster_runner_elects_single_leader() {
        use crate::runner::ClusterRunner;
        use tokio::sync::watch;

        let nodes = make_cluster(&["n1", "n2", "n3"]).await;

        // Keep the sender alive for the whole test: dropping it would resolve
        // `shutdown.changed()` with an error and stop the run loops early.
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let tick = Duration::from_millis(5);
        let mut runners = nodes
            .into_iter()
            .map(|n| ClusterRunner::new(n, tick, shutdown_rx.clone()));
        let mut r1 = runners.next().unwrap();
        let mut r2 = runners.next().unwrap();
        let mut r3 = runners.next().unwrap();

        // run() loops forever, so the 2s timeout branch is what ends the
        // select!. Because select! borrows (does not move) the runners, once
        // the timeout fires and the run() futures are cancelled we regain
        // access to inspect leadership.
        tokio::select! {
            _ = r1.run() => {}
            _ = r2.run() => {}
            _ = r3.run() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }

        let leaders = [r1.is_leader(), r2.is_leader(), r3.is_leader()]
            .into_iter()
            .filter(|&l| l)
            .count();
        assert_eq!(
            leaders, 1,
            "exactly one leader must be elected while ClusterRunner::run() drives the cluster"
        );
    }
}
