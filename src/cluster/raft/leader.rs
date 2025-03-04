/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use super::{Cluster, PeerId};
use super::{State, RAFT_LOG_LEADER};
use crate::cluster::Peer;
use crate::services::{email_delivery, state_change};
use crate::JMAPServer;
use std::sync::atomic::Ordering;
use store::log::raft::TermId;
use store::tracing::debug;
use store::Store;
use tokio::sync::watch;

impl<T> Cluster<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn leader_peer_id(&self) -> Option<PeerId> {
        match self.state {
            State::Leader { .. } => Some(self.peer_id),
            State::Follower { peer_id, .. } => Some(peer_id),
            _ => None,
        }
    }

    pub fn leader_peer(&self) -> Option<&Peer> {
        match self.state {
            State::Follower { peer_id, .. } => self.get_peer(peer_id),
            _ => None,
        }
    }

    pub fn is_leading(&self) -> bool {
        matches!(self.state, State::Leader { .. })
    }

    pub async fn become_leader(&mut self) -> store::Result<()> {
        debug!(
            "[{}] This node is the new leader for term {}.",
            self.addr, self.term
        );

        #[cfg(test)]
        {
            let db_index = self
                .core
                .get_last_log()
                .await?
                .unwrap_or_else(store::log::raft::RaftId::none)
                .index;
            if db_index != self.last_log.index {
                debug!(
                    "Raft index mismatch!!! {} != {}\n",
                    db_index, self.last_log.index
                );
            }
        }

        self.uncommitted_index = self.last_log.index;

        let (event_tx, event_rx) = watch::channel(crate::cluster::leader::Event::new(
            self.last_log.index,
            self.uncommitted_index,
        ));
        let init_rx = self.spawn_raft_leader_init(event_rx.clone());
        self.peers
            .iter()
            .filter(|p| p.is_in_shard(self.shard_id))
            .for_each(|p| {
                self.spawn_raft_leader(
                    p,
                    event_rx.clone(),
                    init_rx.clone().into(),
                    self.config.raft_batch_max,
                )
            });
        self.state = State::Leader {
            tx: event_tx,
            rx: event_rx,
        };
        self.reset_votes();
        Ok(())
    }

    pub fn add_follower(&self, peer_id: PeerId) {
        if let State::Leader { rx, .. } = &self.state {
            self.spawn_raft_leader(
                self.get_peer(peer_id).unwrap(),
                rx.clone(),
                None,
                self.config.raft_batch_max,
            )
        }
    }
}

impl<T> JMAPServer<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub async fn set_leader(&self, term: TermId) {
        // Invalidate caches
        self.store.id_assigner.invalidate_all();
        #[cfg(not(test))]
        {
            self.store.acl_tokens.invalidate_all();
        }
        self.store.recipients.invalidate_all();
        self.store.shared_documents.invalidate_all();

        // Set leader status
        self.store
            .tombstone_deletions
            .store(true, Ordering::Relaxed);
        self.cluster
            .as_ref()
            .unwrap()
            .state
            .store(RAFT_LOG_LEADER, Ordering::Relaxed);
        self.store.raft_term.store(term, Ordering::Relaxed);

        // Start services
        self.state_change
            .clone()
            .send(state_change::Event::Start)
            .await
            .ok();
        self.email_delivery
            .clone()
            .send(email_delivery::Event::Start)
            .await
            .ok();
    }

    pub fn is_leader(&self) -> bool {
        self.cluster
            .as_ref()
            .map(|cluster| cluster.state.load(Ordering::Relaxed) == RAFT_LOG_LEADER)
            .unwrap_or(true)
    }

    #[cfg(test)]
    pub async fn set_leader_term(&self, term: TermId) {
        self.store.raft_term.store(term, Ordering::Relaxed);
        self.store
            .tombstone_deletions
            .store(true, Ordering::Relaxed);
    }
}
