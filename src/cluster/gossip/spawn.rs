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

use crate::authorization::SymmetricEncrypt;
use crate::cluster::Config;

use super::request::Request;
use super::{Event, UDP_MAX_PAYLOAD};
use std::{net::SocketAddr, sync::Arc};
use store::tracing::{debug, error};
use tokio::sync::watch;
use tokio::{net::UdpSocket, sync::mpsc};

/*
  Quidnunc: an inquisitive and gossipy person, from Latin quid nunc? 'what now?'.
  Spawns the gossip process in charge of discovering peers and detecting failures.
*/
pub async fn spawn_quidnunc(
    bind_addr: SocketAddr,
    mut shutdown_rx: watch::Receiver<bool>,
    mut gossip_rx: mpsc::Receiver<(SocketAddr, Request)>,
    main_tx: mpsc::Sender<Event>,
    config: &Config,
) {
    let socket_ = Arc::new(match UdpSocket::bind(bind_addr).await {
        Ok(socket) => socket,
        Err(e) => {
            error!("Failed to bind UDP socket on '{}': {}", bind_addr, e);
            std::process::exit(1);
        }
    });

    // TODO: For the time being nonces are reused since:
    //
    // - No sensitive information is exchanged over UDP (just peer status updates).
    // - Peers need to be authenticated over TLS before joining the cluster.
    // - AES-GCM-SIV is used, which is resistant to nonce reuse.
    //
    // However, it is on the roadmap to use a unique nonce per message, or at
    // least exchange new nonces over TCP periodically.

    let nonce_ = Arc::new(b"428934328968".to_vec());
    let encryptor_ = Arc::new(SymmetricEncrypt::new(
        config.key.as_bytes(),
        "gossipmonger context key",
    ));

    let socket = socket_.clone();
    let encryptor = encryptor_.clone();
    let nonce = nonce_.clone();

    tokio::spawn(async move {
        while let Some((target_addr, response)) = gossip_rx.recv().await {
            // Encrypt packets
            let mut bytes = response.to_bytes();
            match encryptor.encrypt_in_place(&mut bytes, &nonce) {
                Ok(_) => {
                    if let Err(err) = socket.send_to(&bytes, &target_addr).await {
                        error!("Failed to send UDP packet to {}: {}", target_addr, err);
                    }
                }
                Err(err) => {
                    error!("Failed to encrypt UDP packet to {}: {}", target_addr, err);
                }
            }
        }
    });

    let socket = socket_;
    let encryptor = encryptor_;
    let nonce = nonce_;

    tokio::spawn(async move {
        let mut buf = vec![0; UDP_MAX_PAYLOAD];

        loop {
            tokio::select! {
                packet = socket.recv_from(&mut buf) => {
                    match packet {
                        Ok((size, addr)) => {
                            // Decrypt packet
                            match encryptor.decrypt(&buf[..size], &nonce) {
                                Ok(bytes) => {
                                    if let Some(request) = Request::from_bytes(&bytes) {
                                        //debug!("Received packet from {}", addr);
                                        if let Err(e) = main_tx.send(Event::Gossip { addr, request }).await {
                                            error!("Gossip process error, tx.send() failed: {}", e);
                                        }
                                    } else {
                                        debug!("Received invalid gossip message from {}", addr);
                                    }
                                },
                                Err(err) => {
                                    debug!("Failed to decrypt UDP packet from {}: {}", addr, err);
                                },
                            }
                        }
                        Err(e) => {
                            error!("Gossip process ended, socket.recv_from() failed: {}", e);
                        }
                    }
                },
                _ = shutdown_rx.changed() => {
                    debug!("Gossip listener shutting down.");
                    break;
                }
            };
        }
    });
}
