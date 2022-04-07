use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rand::Rng;
use store::log::{LogIndex, RaftId, TermId};
use store::roaring::RoaringTreemap;
use store::tracing::{debug, error, info};
use store::{AccountId, Collection, Store};
use tokio::sync::{mpsc, oneshot, watch};

use crate::cluster::leader;
use crate::JMAPServer;

use super::log::{MergedChanges, RaftStore};
use super::{log, rpc};
use super::{
    rpc::{Request, Response},
    Cluster, Peer, PeerId,
};

pub const ELECTION_TIMEOUT: u64 = 1000;
pub const ELECTION_TIMEOUT_RAND_FROM: u64 = 150;
pub const ELECTION_TIMEOUT_RAND_TO: u64 = 300;

#[derive(Debug)]
pub enum State {
    Leader {
        tx: watch::Sender<leader::Event>,
        rx: watch::Receiver<leader::Event>,
    },
    Wait {
        election_due: Instant,
    },
    Candidate {
        election_due: Instant,
    },
    VotedFor {
        peer_id: PeerId,
        election_due: Instant,
    },
    Follower {
        peer_id: PeerId,
        tx: mpsc::Sender<log::Event>,
    },
}

impl Default for State {
    fn default() -> Self {
        State::Wait {
            election_due: election_timeout(false),
        }
    }
}

impl<T> Cluster<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn has_election_quorum(&self) -> bool {
        let (total, healthy) = self.shard_status();
        healthy >= ((total as f64 + 1.0) / 2.0).floor() as u32
    }

    pub fn is_election_due(&self) -> bool {
        match self.state {
            State::Candidate { election_due }
            | State::Wait { election_due }
            | State::VotedFor { election_due, .. }
                if election_due >= Instant::now() =>
            {
                false
            }
            _ => true,
        }
    }

    pub fn time_to_next_election(&self) -> Option<u64> {
        match self.state {
            State::Candidate { election_due }
            | State::Wait { election_due }
            | State::VotedFor { election_due, .. } => {
                let now = Instant::now();
                Some(if election_due > now {
                    (election_due - now).as_millis() as u64
                } else {
                    0
                })
            }
            _ => None,
        }
    }

    pub fn log_is_behind_or_eq(&self, last_log_term: TermId, last_log_index: LogIndex) -> bool {
        last_log_term > self.last_log.term
            || (last_log_term == self.last_log.term
                && last_log_index.wrapping_add(1) >= self.last_log.index.wrapping_add(1))
    }

    pub fn log_is_behind(&self, last_log_term: TermId, last_log_index: LogIndex) -> bool {
        last_log_term > self.last_log.term
            || (last_log_term == self.last_log.term
                && last_log_index.wrapping_add(1) > self.last_log.index.wrapping_add(1))
    }

    pub fn can_grant_vote(&self, candidate_peer_id: PeerId) -> bool {
        match self.state {
            State::Wait { .. } => true,
            State::VotedFor { peer_id, .. } => candidate_peer_id == peer_id,
            State::Leader { .. } | State::Follower { .. } | State::Candidate { .. } => false,
        }
    }

    pub fn leader_peer_id(&self) -> Option<PeerId> {
        match self.state {
            State::Leader { .. } => Some(self.peer_id),
            State::Follower { peer_id, .. } => Some(peer_id),
            _ => None,
        }
    }

    pub fn is_leading(&self) -> bool {
        matches!(self.state, State::Leader { .. })
    }

    pub fn is_candidate(&self) -> bool {
        matches!(self.state, State::Candidate { .. })
    }

    pub fn is_following(&self) -> bool {
        matches!(self.state, State::Follower { .. })
    }

    pub fn is_following_peer(&self, leader_id: PeerId) -> Option<&mpsc::Sender<log::Event>> {
        match &self.state {
            State::Follower { peer_id, tx } if peer_id == &leader_id => Some(tx),
            _ => None,
        }
    }

    pub fn start_election_timer(&mut self, now: bool) {
        self.state = State::Wait {
            election_due: election_timeout(now),
        };
        self.reset_votes();
        self.core.set_follower();
    }

    pub fn step_down(&mut self, term: TermId) {
        self.reset_votes();
        self.core.set_follower();
        self.term = term;
        self.state = State::Wait {
            election_due: match self.state {
                State::Wait { election_due }
                | State::Candidate { election_due }
                | State::VotedFor { election_due, .. }
                    if election_due < Instant::now() =>
                {
                    election_due
                }
                _ => election_timeout(false),
            },
        };
        debug!("[{}] Stepping down for term {}.", self.addr, self.term);
    }

    pub fn vote_for(&mut self, peer_id: PeerId) {
        self.state = State::VotedFor {
            peer_id,
            election_due: election_timeout(false),
        };
        self.reset_votes();
        self.core.set_follower();
        debug!(
            "[{}] Voted for peer {} for term {}.",
            self.addr,
            self.get_peer(peer_id).unwrap(),
            self.term
        );
    }

    pub async fn follow_leader(
        &mut self,
        peer_id: PeerId,
    ) -> store::Result<mpsc::Sender<log::Event>> {
        let tx = self.spawn_raft_follower();
        self.state = State::Follower {
            peer_id,
            tx: tx.clone(),
        };
        self.reset_votes();
        self.core.set_follower();
        debug!(
            "[{}] Following peer {} for term {}.",
            self.addr,
            self.get_peer(peer_id).unwrap(),
            self.term
        );
        Ok(tx)
    }

    pub fn send_append_entries(&self) {
        if let State::Leader { tx, .. } = &self.state {
            if let Err(err) = tx.send(leader::Event::new(
                self.last_log.index,
                self.uncommitted_index,
            )) {
                error!("Failed to broadcast append entries: {}", err);
            }
        }
    }

    pub fn run_for_election(&mut self, now: bool) {
        self.state = State::Candidate {
            election_due: election_timeout(now),
        };
        self.term += 1;
        self.reset_votes();
        self.core.set_follower();
        debug!(
            "[{}] Running for election for term {}.",
            self.addr, self.term
        );
    }

    pub async fn become_leader(&mut self) -> store::Result<()> {
        debug!(
            "[{}] This node is the new leader for term {}.",
            self.addr, self.term
        );

        // Rollback any uncommitted changes
        self.core.reset_uncommitted_changes().await?;
        self.last_log = self.core.get_last_log().await?.unwrap_or_else(RaftId::none);
        self.uncommitted_index = self.last_log.index;
        self.core.update_raft_index(self.last_log.index);

        let (tx, rx) = watch::channel(leader::Event::new(
            self.last_log.index,
            self.uncommitted_index,
        ));
        self.peers
            .iter()
            .filter(|p| p.is_in_shard(self.shard_id))
            .for_each(|p| self.spawn_raft_leader(p, rx.clone()));
        self.state = State::Leader { tx, rx };
        self.reset_votes();
        self.core
            .set_leader_commit_index(self.last_log.index)
            .await?;
        self.core.set_leader(self.term);
        Ok(())
    }

    pub fn add_follower(&self, peer_id: PeerId) {
        if let State::Leader { rx, .. } = &self.state {
            self.spawn_raft_leader(self.get_peer(peer_id).unwrap(), rx.clone())
        }
    }

    pub fn reset_votes(&mut self) {
        self.peers.iter_mut().for_each(|peer| {
            peer.vote_granted = false;
        });
    }

    pub fn count_vote(&mut self, peer_id: PeerId) -> bool {
        let mut total_peers = 0;
        let shard_id = self.shard_id;
        let mut votes = 1; // Count this node's vote

        self.peers.iter_mut().for_each(|peer| {
            if peer.is_in_shard(shard_id) {
                total_peers += 1;
                if peer.peer_id == peer_id {
                    peer.vote_granted = true;
                    votes += 1;
                } else if peer.vote_granted {
                    votes += 1;
                }
            }
        });

        votes > ((total_peers as f64 + 1.0) / 2.0).floor() as u32
    }

    pub async fn request_votes(&mut self, now: bool) -> store::Result<()> {
        // Check if there is enough quorum for an election.
        if self.has_election_quorum() {
            // Assess whether this node could become the leader for the next term.
            if !self.peers.iter().any(|peer| {
                peer.is_in_shard(self.shard_id)
                    && !peer.is_offline()
                    && self.log_is_behind(peer.last_log_term, peer.last_log_index)
            }) {
                // If this node requires a rollback, it won't be able to become a leader
                // on the next election.
                if !self.core.has_pending_rollback().await? {
                    // Increase term and start election
                    self.run_for_election(now);
                    for peer in &self.peers {
                        if peer.is_in_shard(self.shard_id) && !peer.is_offline() {
                            peer.vote_for_me(self.term, self.last_log.index, self.last_log.term)
                                .await;
                        }
                    }
                } else {
                    self.start_election_timer(now);
                }
            } else {
                // Wait to receive a vote request from a more up-to-date peer.
                debug!(
                    "[{}] Waiting for a vote request from a more up-to-date peer.",
                    self.addr
                );
                self.start_election_timer(now);
            }
        } else {
            self.start_election_timer(false);
            info!(
                "Not enough alive peers in shard {} to start election.",
                self.shard_id
            );
        }

        Ok(())
    }

    pub async fn advance_commit_index(
        &mut self,
        peer_id: PeerId,
        commit_index: LogIndex,
    ) -> store::Result<bool> {
        let mut indexes = Vec::with_capacity(self.peers.len() + 1);
        for peer in self.peers.iter_mut() {
            if peer.is_in_shard(self.shard_id) {
                if peer.peer_id == peer_id {
                    peer.commit_index = commit_index;
                }
                indexes.push(peer.commit_index.wrapping_add(1));
            }
        }
        indexes.push(self.uncommitted_index.wrapping_add(1));
        indexes.sort_unstable();

        // Use div_floor when stabilized.
        let commit_index = indexes[((indexes.len() as f64) / 2.0).floor() as usize];
        if commit_index > self.last_log.index.wrapping_add(1) {
            self.last_log.index = commit_index.wrapping_sub(1);
            self.core
                .set_leader_commit_index(self.last_log.index)
                .await?;
            self.send_append_entries();
            debug!(
                "Advancing commit index to {} [cluster: {:?}].",
                self.last_log.index, indexes
            );
        }
        Ok(true)
    }

    pub fn handle_vote_request(
        &mut self,
        peer_id: PeerId,
        response_tx: oneshot::Sender<rpc::Response>,
        term: TermId,
        last: RaftId,
    ) {
        response_tx
            .send(if self.is_known_peer(peer_id) {
                if self.term < term {
                    self.step_down(term);
                }
                Response::Vote {
                    term: self.term,
                    vote_granted: if self.term == term
                        && self.can_grant_vote(peer_id)
                        && self.log_is_behind_or_eq(last.term, last.index)
                    {
                        self.vote_for(peer_id);
                        true
                    } else {
                        false
                    },
                }
            } else {
                rpc::Response::UnregisteredPeer
            })
            .unwrap_or_else(|_| error!("Oneshot response channel closed."));
    }

    pub async fn handle_vote_response(
        &mut self,
        peer_id: PeerId,
        term: TermId,
        vote_granted: bool,
    ) -> store::Result<()> {
        if self.term < term {
            self.step_down(term);
            return Ok(());
        } else if !self.is_candidate() || !vote_granted || self.term != term {
            return Ok(());
        }

        if self.count_vote(peer_id) {
            self.become_leader().await?;
        }

        Ok(())
    }
}

impl Peer {
    pub async fn vote_for_me(&self, term: TermId, last_log_index: LogIndex, last_log_term: TermId) {
        self.dispatch_request(Request::Vote {
            term,
            last: RaftId::new(last_log_term, last_log_index),
        })
        .await;
    }
}

pub fn election_timeout(now: bool) -> Instant {
    Instant::now()
        + Duration::from_millis(
            if now { 0 } else { ELECTION_TIMEOUT }
                + rand::thread_rng()
                    .gen_range(ELECTION_TIMEOUT_RAND_FROM..ELECTION_TIMEOUT_RAND_TO),
        )
}

impl<T> JMAPServer<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn set_leader(&self, term: TermId) {
        self.is_leader.store(true, Ordering::Relaxed);
        self.is_up_to_date.store(true, Ordering::Relaxed);
        self.store.raft_term.store(term, Ordering::Relaxed);
        self.store.doc_id_cache.invalidate_all();
    }

    pub fn set_follower(&self) {
        self.is_leader.store(false, Ordering::Relaxed);
        self.is_up_to_date.store(false, Ordering::Relaxed);
    }

    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Relaxed)
    }

    pub fn is_up_to_date(&self) -> bool {
        self.is_up_to_date.load(Ordering::Relaxed)
    }

    pub fn set_up_to_date(&self, val: bool) {
        self.is_up_to_date.store(val, Ordering::Relaxed);
    }

    pub fn update_raft_index(&self, index: LogIndex) {
        self.store.raft_index.store(index, Ordering::Relaxed);
    }

    pub async fn get_last_log(&self) -> store::Result<Option<RaftId>> {
        let store = self.store.clone();
        self.spawn_worker(move || store.get_prev_raft_id(RaftId::new(TermId::MAX, LogIndex::MAX)))
            .await
    }

    pub async fn get_prev_raft_id(&self, key: RaftId) -> store::Result<Option<RaftId>> {
        let store = self.store.clone();
        self.spawn_worker(move || store.get_prev_raft_id(key)).await
    }

    pub async fn get_next_raft_id(&self, key: RaftId) -> store::Result<Option<RaftId>> {
        let store = self.store.clone();
        self.spawn_worker(move || store.get_next_raft_id(key)).await
    }

    pub async fn get_raft_match_terms(&self) -> store::Result<Vec<RaftId>> {
        let store = self.store.clone();
        self.spawn_worker(move || store.get_raft_match_terms())
            .await
    }

    pub async fn get_raft_match_indexes(
        &self,
        start_log_index: LogIndex,
    ) -> store::Result<(TermId, RoaringTreemap)> {
        let store = self.store.clone();
        self.spawn_worker(move || store.get_raft_match_indexes(start_log_index))
            .await
    }

    pub async fn prepare_rollback_changes(&self, after_index: LogIndex) -> store::Result<()> {
        let store = self.store.clone();
        self.spawn_worker(move || store.prepare_rollback_changes(after_index))
            .await
    }

    pub async fn next_rollback_change(
        &self,
    ) -> store::Result<Option<(AccountId, Collection, MergedChanges)>> {
        let store = self.store.clone();
        self.spawn_worker(move || store.next_rollback_change())
            .await
    }

    pub async fn remove_rollback_change(
        &self,
        account_id: AccountId,
        collection: Collection,
    ) -> store::Result<()> {
        let store = self.store.clone();
        self.spawn_worker(move || store.remove_rollback_change(account_id, collection))
            .await
    }

    pub async fn reset_uncommitted_changes(&self) -> store::Result<bool> {
        // Rollback uncommitted entries for a previous leader term.
        self.rollback_uncommitted().await?;

        // Apply committed updates and rollback uncommited ones for
        // a previous follower term.
        self.commit_pending_updates().await
    }

    pub async fn has_pending_rollback(&self) -> store::Result<bool> {
        let store = self.store.clone();
        self.spawn_worker(move || store.has_pending_rollback())
            .await
    }

    pub async fn store_changed(&self, last_log: RaftId) {
        if self.is_cluster
            && self
                .cluster_tx
                .send(super::Event::StoreChanged { last_log })
                .await
                .is_err()
        {
            error!("Failed to send store changed event.");
        }
    }

    #[cfg(test)]
    pub async fn notify_changes(&self) {
        self.cluster_tx
            .send(super::Event::StoreChanged {
                last_log: self.get_last_log().await.unwrap().unwrap(),
            })
            .await
            .unwrap();
    }
}
