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

use super::request::Request;
use super::{rpc, Cluster};
use std::net::SocketAddr;
use store::Store;

impl<T> Cluster<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub async fn handle_join(&mut self, id: usize, mut dest: SocketAddr, port: u16) {
        dest.set_port(port);
        self.send_gossip(dest, Request::JoinReply { id }).await;
    }

    pub async fn handle_join_reply(&mut self, id: usize) {
        if let Some(peer) = self.peers.get(id) {
            if peer.is_seed() {
                peer.dispatch_request(rpc::Request::UpdatePeers {
                    peers: self.build_peer_info(),
                })
                .await;
            }
        }
    }
}
