/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/


use core::PEER_VERSION;

use net::PeerAddress;
use net::Neighbor;
use net::NeighborKey;
use net::Error as net_error;
use net::db::PeerDB;
use net::asn::ASEntry4;

use net::*;
use net::codec::*;

use net::connection::Connection;
use net::connection::ConnectionOptions;
use net::connection::NetworkReplyHandle;

use net::db::LocalPeer;

use net::p2p::*;

use util::db::Error as db_error;
use util::db::DBConn;

use util::secp256k1::Secp256k1PublicKey;

use std::net::SocketAddr;
use std::cmp;

use std::collections::HashMap;
use std::collections::HashSet;

use burnchains::Address;
use burnchains::PublicKey;
use burnchains::Burnchain;
use burnchains::BurnchainView;

use util::log;
use util::get_epoch_time_secs;
use util::hash::*;

use rand::seq::SliceRandom;
use rand_os::OsRng;
use rand_os::rand_core::RngCore;

use rusqlite::Transaction;

#[cfg(test)] pub const NEIGHBOR_MINIMUM_CONTACT_INTERVAL : u64 = 0;
#[cfg(not(test))] pub const NEIGHBOR_MINIMUM_CONTACT_INTERVAL : u64 = 600;      // don't reach out to a frontier neighbor more than once every 10 minutes

pub const NEIGHBOR_REQUEST_TIMEOUT : u64 = 60;

pub const NUM_INITIAL_WALKS : u64 = 10;     // how many unthrottled walks should we do when this peer starts up

#[cfg(not(target_arch = "wasm32"))]
impl NeighborKey {
    pub fn from_neighbor_address(peer_version: u32, network_id: u32, na: &NeighborAddress) -> NeighborKey {
        NeighborKey {
            peer_version: peer_version,
            network_id: network_id,
            addrbytes: na.addrbytes.clone(),
            port: na.port
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Neighbor {
    pub fn empty(key: &NeighborKey, pubk: &Secp256k1PublicKey, expire_block: u64) -> Neighbor {
        Neighbor {
            addr: key.clone(),
            public_key: pubk.clone(),
            expire_block: expire_block,
            last_contact_time: 0,
            whitelisted: 0,
            blacklisted: 0,
            asn: 0,
            org: 0,
            in_degree: 1,
            out_degree: 1
        }
    }

    /// Update this peer in the DB.
    /// If there's no DB entry for this peer, then do nothing.
    pub fn save_update<'a>(&self, tx: &mut Transaction<'a>) -> Result<(), net_error> {
        PeerDB::update_peer(tx, &self)
            .map_err(|_e| net_error::DBError)
    }

    /// Save to the peer DB, inserting it if it isn't already there.
    /// Return true if saved.
    /// Return false if not saved -- i.e. the frontier is full and we should try evicting neighbors.
    pub fn save<'a>(&self, tx: &mut Transaction<'a>) -> Result<bool, net_error> {
        PeerDB::try_insert_peer(tx, &self)
            .map_err(|_e| net_error::DBError)
    }

    /// Attempt to load a neighbor from our peer DB, given its NeighborAddress reported by another
    /// peer.  Returns a neighbor in the peer DB if it matches the neighbor address and has a fresh public key
    /// (where "fresh" means "the public key hash matches the neighbor address")
    pub fn from_neighbor_address(conn: &DBConn, peer_version: u32, network_id: u32, block_height: u64, neighbor_address: &NeighborAddress) -> Result<Option<Neighbor>, net_error> {
        let peer_opt = PeerDB::get_peer(conn, network_id, &neighbor_address.addrbytes, neighbor_address.port)
            .map_err(|_e| net_error::DBError)?;

        match peer_opt {
            None => {
                Ok(None)       // unkonwn
            },
            Some(peer) => {
                // expired public key?
                if peer.expire_block < block_height {
                    Ok(None)
                }
                else {
                    let pubkey_160 = Hash160::from_data(&peer.public_key.to_bytes_compressed()[..]);
                    if pubkey_160 == neighbor_address.public_key_hash {
                        // we know this neighbor's key
                        Ok(Some(peer))
                    }
                    else {
                        // this neighbor's key may be stale
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Weighted _undirected_ degree estimate.
    /// If this were an undirected peer graph, the lower bound of a peer's degree would be
    /// min(in-degree, out-degree), and the upper bound would be max(in-degree, out-degree).
    /// Considering that "P1 points to P2" is just as likely as "P2 points to P1", this means that
    /// Pr["P1 points to P2" | "P2 points to P1"] == Pr["P2 points to P1" | "P1 points to P2"].
    /// So, we can estimate the undirected degree as being a random value between the lower and
    /// upper bound.
    pub fn degree(&self) -> u64 {
        let mut rng = OsRng;
        let res = rng.gen_range(self.in_degree, self.out_degree+1) as u64;
        if res == 0 {
            1
        }
        else {
            res
        }
    }
}

/// Struct for capturing the results of a walk.
/// -- reports newly-connected neighbors
/// -- reports neighbors we had trouble talking to.
/// The peer network will use this struct to clean out dead neighbors, and to keep the number of
/// _outgoing_ connections limited to NUM_NEIGHBORS.
#[derive(Clone)]
pub struct NeighborWalkResult {
    pub new_connections: HashSet<NeighborKey>,
    pub broken_connections: HashSet<NeighborKey>,
    pub replaced_neighbors: HashSet<NeighborKey>
}

impl NeighborWalkResult {
    pub fn new() -> NeighborWalkResult {
        NeighborWalkResult {
            new_connections: HashSet::new(),
            broken_connections: HashSet::new(),
            replaced_neighbors: HashSet::new()
        }
    }

    pub fn add_new(&mut self, nk: NeighborKey) -> () {
        self.new_connections.insert(nk);
    }

    pub fn add_broken(&mut self, nk: NeighborKey) -> () {
        self.broken_connections.insert(nk);
    }

    pub fn add_replaced(&mut self, nk: NeighborKey) -> () {
        self.replaced_neighbors.insert(nk);
    }

    pub fn clear(&mut self) -> () {
        self.new_connections.clear();
        self.broken_connections.clear();
        self.replaced_neighbors.clear();
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum NeighborWalkState {
    HandshakeBegin,
    HandshakeFinish,
    GetNeighborsBegin,
    GetNeighborsFinish,
    GetHandshakesBegin,
    GetHandshakesFinish,
    GetNeighborsNeighborsBegin,
    GetNeighborsNeighborsFinish,
    NeighborsPingBegin,
    NeighborsPingFinish,
    Finished
}

pub struct NeighborWalk {
    pub state: NeighborWalkState,
    pub events: HashSet<usize>,

    prev_neighbor: Option<Neighbor>,
    cur_neighbor: Neighbor,
    next_neighbor: Option<Neighbor>,

    pub frontier: HashMap<NeighborKey, Neighbor>,
    new_frontier: HashMap<NeighborKey, Neighbor>,

    // pending request to cur_neighbor to handshake 
    handshake_request: Option<NetworkReplyHandle>,

    // pending request to cur_neighbor to get _its_ neighbors
    getneighbors_request: Option<NetworkReplyHandle>,

    // outstanding requests to handshake with our cur_neighbor's neighbors.
    resolved_handshake_neighbors: HashMap<NeighborAddress, Neighbor>,
    unresolved_handshake_neighbors: HashMap<NeighborAddress, NetworkReplyHandle>,

    // outstanding requests to get the neighbors of our cur_neighbor's neighbors
    resolved_getneighbors_neighbors: HashMap<NeighborKey, Vec<NeighborAddress>>,
    unresolved_getneighbors_neighbors: HashMap<NeighborKey, NetworkReplyHandle>,

    // outstanding requests to ping existing neighbors to be replaced in the frontier
    neighbor_replacements: HashMap<NeighborKey, Neighbor>,
    replaced_neighbors: HashMap<NeighborKey, u32>,
    unresolved_neighbor_pings: HashMap<NeighborKey, NetworkReplyHandle>,

    // neighbor walk result we build up incrementally 
    result: NeighborWalkResult,

    // time that we started/finished the last walk 
    walk_start_time: u64,
    walk_end_time: u64,

    // walk random-restart parameters
    walk_step_count: u64,           // how many times we've taken a step
    walk_min_duration: u64,         // minimum steps we have to take before reset
    walk_max_duration: u64,         // maximum steps we have to take before reset
    walk_reset_prob: f64            // probability that we do a reset once the minimum duration is met
}

#[cfg(not(target_arch = "wasm32"))]
impl NeighborWalk {
    pub fn new(neighbor: &Neighbor) -> NeighborWalk {
        NeighborWalk {
            state: NeighborWalkState::HandshakeBegin,
            events: HashSet::new(),

            prev_neighbor: None,
            cur_neighbor: neighbor.clone(),
            next_neighbor: None,
            
            frontier: HashMap::new(),
            new_frontier: HashMap::new(),
            
            handshake_request: None,
            getneighbors_request: None,

            resolved_handshake_neighbors: HashMap::new(),
            unresolved_handshake_neighbors: HashMap::new(),

            resolved_getneighbors_neighbors: HashMap::new(),
            unresolved_getneighbors_neighbors: HashMap::new(),

            neighbor_replacements: HashMap::new(),
            replaced_neighbors: HashMap::new(),
            unresolved_neighbor_pings: HashMap::new(),

            result: NeighborWalkResult::new(),

            walk_start_time: 0,
            walk_end_time: 0,
            
            walk_step_count: 0,
            walk_min_duration: 20,
            walk_max_duration: 40,
            walk_reset_prob: 0.05,
        }
    }

    /// Reset the walk with a new neighbor.
    /// Give back a report of the walk.
    /// Resets neighbor pointer.
    /// Clears out connections, but preserves state (frontier, result, etc.).
    pub fn reset(&mut self, next_neighbor: &Neighbor) -> NeighborWalkResult {
        test_debug!("Walk reset");
        self.state = NeighborWalkState::HandshakeBegin;

        self.prev_neighbor = Some(self.cur_neighbor.clone());
        self.cur_neighbor = next_neighbor.clone();
        self.next_neighbor = None;

        self.clear_connections();
        self.new_frontier.clear();

        let result = self.result.clone();

        self.walk_end_time = get_epoch_time_secs();
        self.walk_step_count += 1;

        // leave self.frontier and self.result alone until the next walk.
        // (makes it so that at the end of the walk, we can query the result and frontier)
        result
    }

    /// Clear the walk's intermittent state
    pub fn clear_state(&mut self) -> () {
        test_debug!("Walk clear state");
        self.new_frontier.clear();
        self.frontier.clear();
        self.result.clear();
    }

    /// Clear the walk's connection state
    pub fn clear_connections(&mut self) -> () {
        test_debug!("Walk clear connections");
        self.events.clear();
        self.handshake_request = None;
        self.getneighbors_request = None;

        self.resolved_handshake_neighbors.clear();
        self.unresolved_handshake_neighbors.clear();
        
        self.resolved_getneighbors_neighbors.clear();
        self.unresolved_getneighbors_neighbors.clear();

        self.neighbor_replacements.clear();
        self.replaced_neighbors.clear();
        self.unresolved_neighbor_pings.clear();
    }

    /// Update the state of the walk 
    /// (as a separate method for debugging purposes)
    fn set_state(&mut self, local_peer: &LocalPeer, new_state: NeighborWalkState) -> () {
        test_debug!("{:?}: Advance walk state: {:?} --> {:?}", &local_peer, &self.state, &new_state);
        self.state = new_state;
    }

    /// Begin handshaking with our current neighbor 
    pub fn handshake_begin(&mut self, local_peer: &LocalPeer, req: Option<NetworkReplyHandle>) -> () {
        assert!(self.state == NeighborWalkState::HandshakeBegin);

        self.handshake_request = req;

        // next state!
        self.set_state(local_peer, NeighborWalkState::HandshakeFinish);
    }

    /// Finish handshaking with our current neighbor, thereby ensuring that it is connected 
    pub fn handshake_try_finish<'a>(&mut self, tx: &mut Transaction<'a>, local_peer: &LocalPeer, burn_block_height: u64) -> Result<Option<Neighbor>, net_error> {
        assert!(self.state == NeighborWalkState::HandshakeFinish);

        let req_opt = self.handshake_request.take();
        if req_opt.is_none() {
            return Ok(None);
        }

        let req = req_opt.unwrap();
        let handshake_reply_res = req.try_recv();
        match handshake_reply_res {
            Ok(message) => {
                match message.payload {
                    StacksMessageType::HandshakeAccept(ref data) => {
                        // accepted! can proceed to ask for neighbors
                        // save knowledge to the peer DB (NOTE: the neighbor should already be in
                        // the DB, since it's cur_neighbor)
                        test_debug!("{:?}: received HandshakeAccept from {:?}", &local_peer, &message.to_neighbor_key(&data.handshake.addrbytes, data.handshake.port));

                        let neighbor_from_handshake = Neighbor::from_handshake(tx, message.preamble.peer_version, message.preamble.network_id, &data.handshake)?;
                        if neighbor_from_handshake.addr != self.cur_neighbor.addr {
                            // somehow, got a handshake from someone that _isn't_ cur_neighbor
                            debug!("{:?}: got unsolicited HandshakeAccept from {:?} (expected {:?})", &local_peer, &neighbor_from_handshake.addr, &self.cur_neighbor.addr);
                            Err(net_error::PeerNotConnected)
                        }
                        else {
                            // this is indeed cur_neighbor
                            self.cur_neighbor.handshake_update(tx, &data.handshake)?;
                            self.cur_neighbor.save_update(tx)?;
                            
                            self.new_frontier.insert(self.cur_neighbor.addr.clone(), self.cur_neighbor.clone());

                            // advance state!
                            self.set_state(local_peer, NeighborWalkState::GetNeighborsBegin);
                            Ok(Some(self.cur_neighbor.clone()))
                        }
                    },
                    StacksMessageType::HandshakeReject => {
                        // told to bugger off 
                        Err(net_error::PeerNotConnected)
                    },
                    StacksMessageType::Nack(ref data) => {
                        // something's wrong on our end (we're using a new key that they don't yet
                        // know about, or something)
                        Err(net_error::PeerNotConnected)
                    },
                    _ => {
                        // invalid message
                        debug!("{:?}: Got out-of-sequence message from {:?}", &local_peer, &self.cur_neighbor.addr);
                        self.result.add_broken(self.cur_neighbor.addr.clone());
                        Err(net_error::InvalidMessage)
                    }
                }
            },
            Err(req_res) => {
                match req_res {
                    Ok(same_req) => {
                        // try again
                        self.handshake_request = Some(same_req);
                        Ok(None)
                    },
                    Err(e) => {
                        // disconnected 
                        test_debug!("{:?}: failed to get reply: {:?}", &local_peer, &e);
                        self.result.add_broken(self.cur_neighbor.addr.clone());
                        Err(e)
                    }
                }
            }
        }
    }

    /// Begin refreshing our knowledge of peer in/out degrees
    pub fn getneighbors_begin(&mut self, local_peer: &LocalPeer, req: Option<NetworkReplyHandle>) -> () {
        assert!(self.state == NeighborWalkState::GetNeighborsBegin);
        
        self.resolved_handshake_neighbors.clear();
        self.unresolved_handshake_neighbors.clear();
        
        self.getneighbors_request = req;

        // next state!
        self.set_state(local_peer, NeighborWalkState::GetNeighborsFinish);
    }

    /// Find the neighbor addresses that we need to resolve to neighbors,
    /// and find out the neighbor addresses that we already have fresh neighbor data for.
    /// If we know of a neighbor, and contacted it recently, then consider it resolved _even if_
    /// the reported NeighborAddress public key hash doesn't match our records.
    fn lookup_stale_neighbors(dbconn: &DBConn, peer_version: u32, network_id: u32, block_height: u64, addrs: &Vec<NeighborAddress>) -> Result<(HashMap<NeighborAddress, Neighbor>, Vec<NeighborAddress>), net_error> {
        let mut to_resolve = vec![];
        let mut resolved = HashMap::<NeighborAddress, Neighbor>::new();
        for naddr in addrs {
            let neighbor_opt = Neighbor::from_neighbor_address(dbconn, peer_version, network_id, block_height, naddr)?;
            match neighbor_opt {
                None => {
                    // need to resolve this one, but don't talk to it if we did so recently (even
                    // if we have stale information for it -- the remote node could be trying to trick
                    // us into DDoS'ing this node).
                    let peer_opt = PeerDB::get_peer(dbconn, network_id, &naddr.addrbytes, naddr.port)
                        .map_err(|_e| net_error::DBError)?;

                    match peer_opt {
                        None => {
                            // okay, we really don't know about this neighbor
                            to_resolve.push((*naddr).clone());
                        },
                        Some(n) => {
                            // we know about this neighbor, but its key didn't match the
                            // neighboraddress.  Only try to re-connect with it if we haven't done
                            // so recently, so a rogue neighbor can't force us to DDoS another
                            // peer.
                            if n.last_contact_time + NEIGHBOR_MINIMUM_CONTACT_INTERVAL < get_epoch_time_secs() {
                                to_resolve.push((*naddr).clone());
                            }
                            else {
                                // recently contacted
                                resolved.insert(naddr.clone(), n);
                            }
                        }
                    }
                }
                Some(neighbor) => {
                    if neighbor.last_contact_time + NEIGHBOR_MINIMUM_CONTACT_INTERVAL < get_epoch_time_secs() {
                        // stale 
                        to_resolve.push((*naddr).clone());
                    }
                    else {
                        // our copy is still fresh 
                        resolved.insert(naddr.clone(), neighbor);
                    }
                }
            }
        }
        Ok((resolved, to_resolve))
    }

    /// Try to finish the getneighbors request to cur_neighbor
    /// Returns the list of neighbors we need to resolve
    /// Return None if we're not done yet, or haven't started yet.
    pub fn getneighbors_try_finish(&mut self, dbconn: &DBConn, local_peer: &LocalPeer, block_height: u64) -> Result<Option<Vec<NeighborAddress>>, net_error> {
        assert!(self.state == NeighborWalkState::GetNeighborsFinish);

        let req_opt = self.getneighbors_request.take();
        if req_opt.is_none() {
            return Ok(None);
        }

        let req = req_opt.unwrap();
        let neighbors_reply_res = req.try_recv();
        match neighbors_reply_res {
            Ok(message) => {
                match message.payload {
                    StacksMessageType::Neighbors(ref data) => {
                        let (mut found, to_resolve) = NeighborWalk::lookup_stale_neighbors(dbconn, message.preamble.peer_version, message.preamble.network_id, block_height, &data.neighbors)?;

                        for (naddr, neighbor) in found.drain() {
                            self.new_frontier.insert(neighbor.addr.clone(), neighbor.clone());
                            self.resolved_handshake_neighbors.insert(naddr, neighbor);
                        }

                        self.set_state(local_peer, NeighborWalkState::GetHandshakesBegin);
                        Ok(Some(to_resolve))
                    },
                    StacksMessageType::Nack(ref data) => {
                        debug!("Neighbor {:?} NACK'ed GetNeighbors with code {:?}", &self.cur_neighbor.addr, data.error_code);
                        self.result.add_broken(self.cur_neighbor.addr.clone());
                        Err(net_error::ConnectionBroken)
                    },
                    _ => {
                        // invalid message
                        debug!("Got out-of-sequence message from {:?}", &self.cur_neighbor.addr);
                        self.result.add_broken(self.cur_neighbor.addr.clone());
                        Err(net_error::InvalidMessage)
                    }
                }
            },
            Err(req_res) => {
                match req_res {
                    Ok(same_req) => {
                        // try again
                        self.getneighbors_request = Some(same_req);
                        Ok(None)
                    },
                    Err(e) => {
                        // disconnected 
                        self.result.add_broken(self.cur_neighbor.addr.clone());
                        Err(e)
                    }
                }
            }
        }
    }

    /// Begin getting the neighors of cur_neighbor's neighbors.
    /// NetworkReplyHandles should be reply handles for Handshake requests.
    pub fn neighbor_handshakes_begin(&mut self, local_peer: &LocalPeer, mut handshake_handles: HashMap<NeighborAddress, NetworkReplyHandle>) -> () {
        assert!(self.state == NeighborWalkState::GetHandshakesBegin);

        // advance state!
        self.unresolved_handshake_neighbors.clear();
        for (naddr, nh) in handshake_handles.drain() {
            self.unresolved_handshake_neighbors.insert(naddr, nh);
        }

        self.set_state(local_peer, NeighborWalkState::GetHandshakesFinish);
    }

    /// Given a neighbor we tried to insert into the peer database, find one of the existing
    /// neighbors it collided with.  Return its slot in the peer db.
    fn find_replaced_neighbor_slot(conn: &DBConn, nk: &NeighborKey) -> Result<Option<u32>, net_error> {
        let mut slots = PeerDB::peer_slots(conn, nk.network_id, &nk.addrbytes, nk.port)
            .map_err(|_e| net_error::DBError)?;

        if slots.len() == 0 {
            // not present
            return Ok(None);
        }

        let mut rng = OsRng;
        slots.shuffle(&mut rng);
        
        for slot in slots {
            let peer_opt = PeerDB::get_peer_at(conn, nk.network_id, slot)
                .map_err(|_e| net_error::DBError)?;

            match peer_opt {
                None => {
                    continue;
                }
                Some(_) => {
                    return Ok(Some(slot));
                }
            }
        }

        Ok(None)
    }


    /// Try to finish getting handshakes from cur_neighbors' neighbors.
    /// Once all handles resolve, return the list of neighbors that we can contact.
    /// As a side-effect of handshaking with all these peers, our PeerDB instance will be expanded
    /// with the addresses, public keys, public key expiries of these neighbors -- i.e. this method grows
    /// our frontier.
    pub fn neighbor_handshakes_try_finish<'a>(&mut self, tx: &mut Transaction<'a>, local_peer: &LocalPeer, block_height: u64) -> Result<Option<Vec<NeighborKey>>, net_error> {
        assert!(self.state == NeighborWalkState::GetHandshakesFinish);

        // see if we got any replies 
        let mut new_unresolved_handshakes = HashMap::new();
        for (naddr, rh) in self.unresolved_handshake_neighbors.drain() {
            let res = rh.try_recv();
            let rh_naddr = naddr.clone();       // used below
            let new_rh = match res {
                Ok(message) => {
                    match message.payload {
                        StacksMessageType::HandshakeAccept(ref data) => {
                            // success! do we know about this peer already?
                            let neighbor_from_handshake = Neighbor::from_handshake(tx, message.preamble.peer_version, message.preamble.network_id, &data.handshake)?;
                            let mut neighbor_opt = Neighbor::from_neighbor_address(tx, message.preamble.peer_version, message.preamble.network_id, block_height, &naddr)?;
                            match neighbor_opt {
                                Some(neighbor) => {
                                    test_debug!("{:?}: already know about {:?}", &local_peer, &neighbor.addr);

                                    // knew about this neighbor already
                                    self.resolved_handshake_neighbors.insert(naddr, neighbor.clone());

                                    // update our frontier as well
                                    self.new_frontier.insert(neighbor.addr.clone(), neighbor);
                                    neighbor_from_handshake.save_update(tx)?;
                                },
                                None => {
                                    test_debug!("{:?}: new neighbor {:?}", &local_peer, &neighbor_from_handshake.addr);

                                    // didn't know about this neighbor yet. Try to add it.
                                    let added = neighbor_from_handshake.save(tx)?;
                                    if !added {
                                        // no more room in the db.  See if we can add it by
                                        // evicting an existing neighbor once we're done with this
                                        // walk.
                                        let replaced_neighbor_slot_opt = NeighborWalk::find_replaced_neighbor_slot(tx, &neighbor_from_handshake.addr)?;

                                        match replaced_neighbor_slot_opt {
                                            Some(slot) => {
                                                // if this peer isn't whitelisted, then consider
                                                // replacing
                                                if neighbor_from_handshake.whitelisted > 0 && (neighbor_from_handshake.whitelisted as u64) < get_epoch_time_secs() {
                                                    self.neighbor_replacements.insert(neighbor_from_handshake.addr.clone(), neighbor_from_handshake.clone());
                                                    self.replaced_neighbors.insert(neighbor_from_handshake.addr.clone(), slot);
                                                }
                                            },
                                            None => {
                                                // shouldn't happen 
                                            }
                                        };
                                    }
                                    self.new_frontier.insert(neighbor_from_handshake.addr.clone(), neighbor_from_handshake);
                                }
                            };
                        },
                        StacksMessageType::HandshakeReject => {
                            // remote peer doesn't want to talk to us 
                            debug!("Neighbor {:?} rejected our handshake", &naddr);
                            self.result.add_broken(NeighborKey::from_neighbor_address(message.preamble.peer_version, message.preamble.network_id, &naddr));
                        },
                        StacksMessageType::Nack(ref data) => {
                            // remote peer nope'd us
                            debug!("Neighbor {:?} NACK'ed our handshake with error code {:?}", &naddr, data.error_code);
                            self.result.add_broken(NeighborKey::from_neighbor_address(message.preamble.peer_version, message.preamble.network_id, &naddr));
                        }
                        _ => {
                            // remote peer doesn't want to talk to us
                            debug!("Neighbor {:?} replied an out-of-sequence message", &naddr);
                            self.result.add_broken(NeighborKey::from_neighbor_address(message.preamble.peer_version, message.preamble.network_id, &naddr));
                        }
                    };
                    None
                },
                Err(req_res) => {
                    match req_res {
                        Ok(same_req) => {
                            // try again 
                            Some(same_req)
                        },
                        Err(e) => {
                            // connection broken.
                            // Don't try to contact this node again.
                            debug!("Failed to handshake with {:?}: {:?}", naddr, &e);
                            self.result.add_broken(NeighborKey::from_neighbor_address(PEER_VERSION, local_peer.network_id, &naddr));
                            None
                        }
                    }
                }
            };
            match new_rh {
                Some(rh) => {
                    new_unresolved_handshakes.insert(rh_naddr, rh);
                },
                None => {}
            };
        }

        // save unresolved handshakes for next time 
        for (naddr, rh) in new_unresolved_handshakes.drain() {
            self.unresolved_handshake_neighbors.insert(naddr, rh);
        }

        if self.unresolved_handshake_neighbors.len() == 0 {
            // finished handshaking!  find neighbors that accepted
            let mut neighbor_keys = vec![];
            
            // update our frontier knowledge
            for (nkey, new_neighbor) in self.new_frontier.drain() {
                test_debug!("{:?}: Add to frontier: {:?}", &local_peer, &nkey);
                self.frontier.insert(nkey.clone(), new_neighbor);

                if nkey.addrbytes != self.cur_neighbor.addr.addrbytes || nkey.port != self.cur_neighbor.addr.port {
                    neighbor_keys.push(nkey.clone());
                }
            }

            self.new_frontier.clear();

            // advance state!
            self.set_state(local_peer, NeighborWalkState::GetNeighborsNeighborsBegin);
            Ok(Some(neighbor_keys))
        }
        else {
            // still handshaking 
            Ok(None)
        }
    }

    /// Begin asking remote neighbors for their neighbors in order to estimate cur_neighbor's
    /// in-degree. 
    pub fn getneighbors_neighbors_begin(&mut self, local_peer: &LocalPeer, mut getneighbors_handles: HashMap<NeighborKey, NetworkReplyHandle>) -> () {
        assert!(self.state == NeighborWalkState::GetNeighborsNeighborsBegin);

        // advance state!
        self.unresolved_getneighbors_neighbors.clear();
        for (naddr, nh) in getneighbors_handles.drain() {
            self.unresolved_getneighbors_neighbors.insert(naddr, nh);
        }

        self.set_state(local_peer, NeighborWalkState::GetNeighborsNeighborsFinish);
    }

    /// Try to finish getting the neighbors from cur_neighbors' neighbors 
    /// Once all handles resolve, return the list of new neighbors.
    pub fn getneighbors_neighbors_try_finish<'a>(&mut self, tx: &mut Transaction<'a>, local_peer: &LocalPeer) -> Result<Option<Neighbor>, net_error> {
        assert!(self.state == NeighborWalkState::GetNeighborsNeighborsFinish);

        // see if we got any replies 
        let mut new_unresolved_neighbors = HashMap::new();
        for (nkey, rh) in self.unresolved_getneighbors_neighbors.drain() {
            let rh_nkey = nkey.clone();     // used below
            let res = rh.try_recv();
            let new_rh = match res {
                Ok(message) => {
                    match message.payload {
                        StacksMessageType::Neighbors(ref data) => {
                            self.resolved_getneighbors_neighbors.insert(nkey, data.neighbors.clone());
                        },
                        StacksMessageType::Nack(ref data) => {
                            // not broken; likely because it hasn't gotten to processing our
                            // handshake yet.  We'll just ignore it.
                            debug!("Neighbor {:?} NACKed with code {:?}", &nkey, data.error_code);
                        },
                        _ => {
                            // unexpected reply
                            debug!("Neighbor {:?} replied an out-of-sequence message (type {}); assuming broken", &nkey, message_type_to_id(&message.payload));
                            self.result.add_broken(nkey);
                        }
                    };
                    None
                },
                Err(req_res) => {
                    match req_res {
                        Ok(nrh) => {
                            // try again 
                            Some(nrh)
                        }
                        Err(e) => {
                            // disconnected from peer 
                            debug!("Failed to get neighbors from {:?}", &nkey);
                            self.result.add_broken(nkey);
                            None
                        }
                    }
                }
            };
            match new_rh {
                Some(rh) => {
                    new_unresolved_neighbors.insert(rh_nkey, rh);
                },
                None => {}
            };
        }

        // try these again 
        for (nkey, rh) in new_unresolved_neighbors.drain() {
            test_debug!("{:?}: still waiting for Neighbors reply from {:?}", &local_peer, &nkey);
            self.unresolved_getneighbors_neighbors.insert(nkey, rh);
        }

        if self.unresolved_getneighbors_neighbors.len() == 0 {
            // finished!  build up frontier's in-degree estimation, plus ourselves
            self.cur_neighbor.in_degree = 1;
            self.cur_neighbor.out_degree = self.frontier.len() as u32;

            for (nkey, neighbor_list) in self.resolved_getneighbors_neighbors.iter() {
                for na in neighbor_list {
                    if na.addrbytes == self.cur_neighbor.addr.addrbytes && na.port == self.cur_neighbor.addr.port {
                        self.cur_neighbor.in_degree += 1;
                    }
                }
            }

            // remember this peer's in/out degree estimates
            test_debug!("{:?}: In/Out degree of {:?} is {}/{}", &local_peer, &self.cur_neighbor.addr, self.cur_neighbor.in_degree, self.cur_neighbor.out_degree);
            self.cur_neighbor.save_update(tx)
                .map_err(|e| net_error::DBError)?;

            // advance state!
            self.set_state(local_peer, NeighborWalkState::NeighborsPingBegin);
            Ok(Some(self.cur_neighbor.clone()))
        }
        else {
            // still working
            Ok(None)
        }
    }

    /// Pick a random neighbor from the frontier, excluding an optional given neighbor 
    fn pick_random_neighbor(frontier: &HashMap<NeighborKey, Neighbor>, exclude: Option<&Neighbor>) -> Option<Neighbor> {
        let mut rnd = OsRng;

        use rand::Rng;
        let sample = rnd.gen_range(0, frontier.len());
        let mut count = 0;

        for (nk, n) in frontier.iter() {
            count += match exclude {
                None => 1,
                Some(ref e) => if (*e).addr == *nk { 0 } else { 1 }
            };
            if count >= sample {
                return Some(n.clone());
            }
        }
        return None;
    }
    
    /// Calculate the "degree ratio" between two neighbors, used to determine the probability of
    /// stepping to a neighbor in MHRWDA.  We estimate each neighbor's undirected degree, and then
    /// measure how represented each neighbor's AS is in the peer graph.  We *bias* the sample so
    /// that peers in under-represented ASs are more likely to be walked to than they otherwise
    /// would be if considering only neighbor degrees.
    fn degree_ratio(peerdb_conn: &DBConn, n1: &Neighbor, n2: &Neighbor) -> f64 {
        let d1 = n1.degree() as f64;
        let d2 = n2.degree() as f64;
        let as_d1 = PeerDB::asn_count(peerdb_conn, n1.asn).unwrap_or(1) as f64;
        let as_d2 = PeerDB::asn_count(peerdb_conn, n2.asn).unwrap_or(1) as f64;
        (d1 * as_d2) / (d2 * as_d1)
    }

    /// Do the MHRWDA step -- try to step from our cur_neighbor to an immediate neighbor, if there
    /// is any neighbor to step to.  Return the new cur_neighbor, if we were able to step.
    /// The caller should call reset() after this, optionally with a newly-selected frontier
    /// neighbor if we were unable to take a step.
    ///
    /// This is a slightly modified MHRWDA algorithm.  The following differences are described:
    /// * The Stacks peer network is a _directed_ graph, whereas MHRWDA is desigend to operate
    /// on _undirected_ graphs.  As such, we calculate a separate peer graph with undirected edges
    /// with the same peers.  We estimate a peer's undirected degree with Neighbor::degree().
    /// * The probability of transitioning to a new peer is proportional not only to the ratio of
    /// the current peer's degree to the new peer's degree, but also to the ratio of the new
    /// peer's AS's node count to the current peer's AS's node count.
    pub fn step(&mut self, peerdb_conn: &DBConn) -> Option<Neighbor> {
        let mut rnd = OsRng;

        // step to a node in cur_neighbor's frontier, per MHRWDA
        let next_neighbor_opt = 
            if self.frontier.len() == 0 {
                // just started the walk, so stay here for now -- we don't yet know the neighbor's
                // frontier.
                Some(self.cur_neighbor.clone())
            }
            else {
                let next_neighbor = NeighborWalk::pick_random_neighbor(&self.frontier, None).unwrap();     // won't panic since self.frontier.len() > 0
                let walk_prob : f64 = rnd.gen();
                if walk_prob < fmin!(1.0, NeighborWalk::degree_ratio(peerdb_conn, &self.cur_neighbor, &next_neighbor)) {
                    match self.prev_neighbor {
                        Some(ref prev_neighbor) => {
                            // will take a step
                            if prev_neighbor.addr == next_neighbor.addr {
                                // oops, backtracked.  Try to pick a different neighbor, if possible.
                                if self.frontier.len() == 1 {
                                    // no other choices. will need to reset this walk.
                                    None
                                }
                                else {
                                    // have alternative choices, so instead of backtracking, we'll delay
                                    // acceptance by probabilistically deciding to step to an alternative
                                    // instead of backtracking.
                                    let alt_next_neighbor = NeighborWalk::pick_random_neighbor(&self.frontier, Some(&prev_neighbor)).unwrap();
                                    let alt_prob : f64 = rnd.gen();

                                    let cur_to_alt = NeighborWalk::degree_ratio(peerdb_conn, &self.cur_neighbor, &alt_next_neighbor);
                                    let prev_to_cur = NeighborWalk::degree_ratio(peerdb_conn, &prev_neighbor, &self.cur_neighbor);
                                    let trans_prob = fmin!(
                                                        fmin!(1.0, cur_to_alt * cur_to_alt),
                                                        fmax!(1.0, prev_to_cur * prev_to_cur)
                                                     );

                                    if alt_prob < fmin!(1.0, trans_prob) {
                                        // go to alt peer instead
                                        Some(alt_next_neighbor)
                                    }
                                    else {
                                        // backtrack.
                                        Some(next_neighbor)
                                    }
                                }
                            }
                            else {
                                // not backtracking.  Take a step.
                                Some(next_neighbor)
                            }
                        },
                        None => {
                            // not backtracking.  Take a step.
                            Some(next_neighbor)
                        }
                    }
                }
                else {
                    // will not take a step
                    Some(self.cur_neighbor.clone())
                }
            };

        self.next_neighbor = next_neighbor_opt.clone();
        next_neighbor_opt
    }

    // proceed to ping _existing_ neighbors that would be replaced by the discovery of a new
    // neighbor
    pub fn ping_existing_neighbors_begin(&mut self, local_peer: &LocalPeer, mut network_handles: HashMap<NeighborKey, NetworkReplyHandle>) -> () {
        assert!(self.state == NeighborWalkState::NeighborsPingBegin);

        self.unresolved_neighbor_pings.clear();

        for (neighbor_key, ping_handle) in network_handles.drain() {
            self.unresolved_neighbor_pings.insert(neighbor_key, ping_handle);
        }

        // advance state!
        self.set_state(local_peer, NeighborWalkState::NeighborsPingFinish);
    }

    // try to finish pinging/handshaking all exisitng neighbors.
    // if the remote neighbor does _not_ respond to our ping, then replace it.
    // Return the list of _evicted_ neighbors.
    pub fn ping_existing_neighbors_try_finish<'a>(&mut self, tx: &mut Transaction<'a>, local_peer: &LocalPeer, network_id: u32) -> Result<Option<HashSet<NeighborKey>>, net_error> {
        assert!(self.state == NeighborWalkState::NeighborsPingFinish);

        let mut new_unresolved_neighbor_pings = HashMap::new();
        
        for (nkey, rh) in self.unresolved_neighbor_pings.drain() {
            let rh_nkey = nkey.clone();     // used below
            let res = rh.try_recv();
            let new_rh = match res {
                Ok(message) => {
                    match message.payload {
                        StacksMessageType::HandshakeAccept(ref data) => {
                            // this peer is still alive -- will not replace it 
                            // save knowledge to the peer DB (NOTE: the neighbor should already be in
                            // the DB, since it's cur_neighbor)
                            test_debug!("{:?}: received HandshakeAccept from {:?}", &local_peer, &message.to_neighbor_key(&data.handshake.addrbytes, data.handshake.port));

                            let neighbor_from_handshake = Neighbor::from_handshake(tx, message.preamble.peer_version, message.preamble.network_id, &data.handshake)?;
                            neighbor_from_handshake.save_update(tx)?;

                            // not going to replace
                            if self.replaced_neighbors.contains_key(&neighbor_from_handshake.addr) {
                                test_debug!("{:?}: will NOT replace {:?}", &local_peer, &neighbor_from_handshake.addr);
                                self.replaced_neighbors.remove(&neighbor_from_handshake.addr);
                            }
                        },
                        StacksMessageType::Nack(ref data) => {
                            // evict
                            debug!("Neighbor {:?} NACK'ed Handshake with code {:?}; will evict", nkey, data.error_code);
                            self.result.add_broken(nkey.clone());
                        },
                        _ => {
                            // unexpected reply -- this peer is misbehaving and should be replaced
                            debug!("Neighbor {:?} replied an out-of-sequence message (type {}); will replace", &nkey, message_type_to_id(&message.payload));
                            self.result.add_broken(nkey);
                        }
                    };
                    None
                },
                Err(req_res) => {
                    match req_res {
                        Ok(nrh) => {
                            // try again 
                            Some(nrh)
                        }
                        Err(e) => {
                            // disconnected from peer already -- we can replace it
                            debug!("Neighbor {:?} could not be pinged; will replace", &nkey);
                            self.result.add_broken(nkey);
                            None
                        }
                    }
                }
            };
            match new_rh {
                Some(rh) => {
                    // try again next time
                    new_unresolved_neighbor_pings.insert(rh_nkey, rh);
                },
                None => {}
            };
        }

        if new_unresolved_neighbor_pings.len() == 0 {
            // done getting pings.  do our replacements
            for (replaceable_key, slot) in self.replaced_neighbors.iter() {
                let replacement = match self.neighbor_replacements.get(replaceable_key) {
                    Some(n) => n.clone(),
                    None => {
                        continue;
                    }
                };

                let replaced_opt = PeerDB::get_peer_at(tx, network_id, *slot)
                    .map_err(|_e| net_error::DBError)?;

                match replaced_opt {
                    Some(replaced) => {
                        debug!("Replace {:?} with {:?}", &replaced.addr, &replacement.addr);

                        PeerDB::insert_or_replace_peer(tx, &replacement, *slot)
                            .map_err(|_e| net_error::DBError)?;

                        self.result.add_replaced(replaced.addr.clone());
                    },
                    None => {}
                }
            }

            // advance state!
            self.set_state(local_peer, NeighborWalkState::Finished);
            Ok(Some(self.result.replaced_neighbors.clone()))
        }
        else {
            // still have more work to do
            self.unresolved_neighbor_pings = new_unresolved_neighbor_pings;
            Ok(None)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl PeerNetwork {
    /// Get some initial fresh random neighbor(s) to crawl
    pub fn get_random_neighbors(&self, num_neighbors: u64, block_height: u64) -> Result<Vec<Neighbor>, net_error> {
        let neighbors = PeerDB::get_random_walk_neighbors(&self.peerdb.conn(), self.burnchain.network_id, num_neighbors as u32, block_height)
            .map_err(|_e| net_error::DBError)?;

        if neighbors.len() == 0 {
            debug!("No neighbors available!");
            return Err(net_error::NoSuchNeighbor);
        }
        Ok(neighbors)
    }

    /// Connect to a remote peer and begin to handshake with it.
    fn connect_and_handshake(&mut self, walk: &mut NeighborWalk, local_peer: &LocalPeer, chain_view: &BurnchainView, nk: &NeighborKey) -> Result<NetworkReplyHandle, net_error> {
        if !self.is_registered(nk) {
            let con_res = self.connect_peer(&local_peer, chain_view, nk);
            match con_res {
                Ok(event_id) => {
                    // remember this in the walk result
                    walk.result.add_new(nk.clone());

                    // stop the pruner from removing this connection
                    walk.events.insert(event_id);
                },
                Err(e) => {
                    test_debug!("{:?}: Failed to connect to {:?}: {:?}", &local_peer, nk, &e);
                    return Err(net_error::PeerNotConnected);
                }
            }
        }
        else {
            test_debug!("{:?}: already connected to {:?} as event {}", &local_peer, &nk, self.get_event_id(nk).unwrap());
        }

        // so far so good.
        // send handshake.
        let handshake_data = HandshakeData::from_local_peer(local_peer);
        
        test_debug!("{:?}: send Handshake to {:?}", &local_peer, &nk);

        let msg = self.sign_for_peer(local_peer, chain_view, nk, StacksMessageType::Handshake(handshake_data))?;
        let req_res = self.send_message(nk, msg, get_epoch_time_secs() + NEIGHBOR_REQUEST_TIMEOUT);
        match req_res {
            Ok(handle) => {
                Ok(handle)
            },
            Err(e) => {
                debug!("Not connected: {:?} ({:?}", nk, &e);
                walk.result.add_broken(nk.clone());
                Err(net_error::PeerNotConnected)
            }
        }
    }

    /// Instantiate the neighbor walk 
    fn instantiate_walk(&mut self, chain_view: &BurnchainView) -> Result<(), net_error> {
        // pick a random neighbor as a walking point 
        let next_neighbors = self.get_random_neighbors(1, chain_view.burn_block_height)?;
        let mut w = NeighborWalk::new(&next_neighbors[0]);
        w.walk_start_time = get_epoch_time_secs();

        self.walk = Some(w);
        Ok(())
    }


    /// Begin walking the peer graph by reaching out to a neighbor and handshaking with it.
    /// Return an error to reset the walk.
    pub fn walk_handshake_begin(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<(), net_error> {
        if self.walk.is_none() {
            self.instantiate_walk(chain_view)?;
        }
        
        // We have to jump thru a few hoops to access self and walk mutably.
        // also, we want "try/catch"-like behavior, where we can capture
        // an error returned by the `?` operator.  To do this, we put the
        // body of this method in a closure as follows.
        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => unreachable!(),
                    Some(ref mut walk) => {
                        match walk.handshake_request {
                            Some(_) => {
                                // in progress already
                                Ok(())
                            },
                            None => {
                                let my_addr = walk.cur_neighbor.addr.clone();
                                walk.clear_state();

                                let handle = self.connect_and_handshake(walk, local_peer, chain_view, &my_addr)?;
                                walk.handshake_begin(local_peer, Some(handle));
                                Ok(())
                            }
                        }
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Try to finish handshaking with our current neighbor
    pub fn walk_handshake_try_finish(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<Option<Neighbor>, net_error> {
        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => {
                        panic!("Invalid neighbor-walk state reached -- cannot finish getting neighbors when the walk state is not instantiated");
                    },
                    Some(ref mut walk) => {
                        let neighbor_opt = {
                            let mut tx = self.peerdb.tx_begin()
                                .map_err(|_e| net_error::DBError)?;

                            let res = walk.handshake_try_finish(&mut tx, local_peer, chain_view.burn_block_height)?;
                            tx.commit()
                                .map_err(|_e| net_error::DBError)?;

                            res
                        };
                        Ok(neighbor_opt)
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Begin walking the peer graph by reaching out to a neighbor, connecting to _it's_ neighbors,
    /// asking for their neighbor-sets (in order to get the neighbor's in/out-degree estimates),
    /// and then stepping to one of the neighbor's neighbors.
    /// Return an error to reset the walk.
    pub fn walk_getneighbors_begin(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<(), net_error> {
        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => {
                        panic!("Invalid neighbor-walk state reached -- cannot finish getting our neighbor's neighbors when the walk state is not instantiated");
                    },
                    Some(ref mut walk) => {
                        match walk.getneighbors_request {
                            Some(_) => {
                                Ok(())
                            },
                            None => {
                                test_debug!("{:?}: send GetNeighbors to {:?}", &local_peer, &walk.cur_neighbor);

                                let msg = self.sign_for_peer(local_peer, chain_view, &walk.cur_neighbor.addr, StacksMessageType::GetNeighbors)?;
                                let req_res = self.send_message(&walk.cur_neighbor.addr, msg, get_epoch_time_secs() + NEIGHBOR_REQUEST_TIMEOUT);
                                match req_res {
                                    Ok(handle) => {
                                        walk.getneighbors_begin(local_peer, Some(handle));
                                        Ok(())
                                    },
                                    Err(e) => {
                                        debug!("Not connected: {:?} ({:?}", &walk.cur_neighbor.addr, &e);
                                        Err(e)
                                    }
                                }
                            }
                        }
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Make progress completing the pending getneighbor request, and if it completes,
    /// proceed to handshake with all its neighbors that we don't know about.
    /// Return an error to reset the walk.
    pub fn walk_getneighbors_try_finish(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<(), net_error> {
        let my_pubkey_hash = Hash160::from_data(&Secp256k1PublicKey::from_private(&local_peer.private_key).to_bytes()[..]);

        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => {
                        panic!("Invalid neighbor-walk state reached -- cannot finish getting neighbors when the walk state is not instantiated");
                    },
                    Some(ref mut walk) => {
                        let cur_neighbor_pubkey_hash = Hash160::from_data(&walk.cur_neighbor.public_key.to_bytes_compressed()[..]);
                        let neighbor_addrs_opt = walk.getneighbors_try_finish(self.peerdb.conn(), local_peer, chain_view.burn_block_height)?;
                        match neighbor_addrs_opt {
                            None => {
                                // nothing to do -- not done yet
                                Ok(())
                            },
                            Some(neighbor_addrs) => {
                                // got neighbors -- proceed to ask each one for *its* neighbors so we can
                                // estimate cur_neighbor's in-degree and grow our frontier.
                                let mut pending_handshakes = HashMap::new();
                                let now = get_epoch_time_secs();

                                for na in neighbor_addrs {
                                    // don't talk to myself if we're listed as a neighbor of this
                                    // remote peer.
                                    if na.public_key_hash == my_pubkey_hash {
                                        continue;
                                    }

                                    // don't handshake with cur_neighbor, if for some reason it gets listed
                                    // in the neighbors reply
                                    if na.public_key_hash == cur_neighbor_pubkey_hash {
                                        continue;
                                    }

                                    let nk = NeighborKey::from_neighbor_address(self.burnchain.peer_version, self.burnchain.network_id, &na);
                                    let handle_res = self.connect_and_handshake(walk, local_peer, chain_view, &nk);
                                    match handle_res {
                                        Ok(handle) => {
                                            pending_handshakes.insert(na, handle);
                                        }
                                        Err(e) => {
                                            continue;
                                        }
                                    }
                                }

                                walk.neighbor_handshakes_begin(local_peer, pending_handshakes);
                                Ok(())
                            }
                        }
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Make progress on completing handshakes with all our neighbors.  If we finish, proceed to
    /// ask them for their neighbors in order to estimate cur_neighbor's in/out degrees.
    /// Return an error to reset the walk.
    pub fn walk_neighbor_handshakes_try_finish(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<(), net_error> {
        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => {
                        panic!("Invalid neighbor-walk state reached -- cannot finish handshaking with neighbor's frontier when the walk state is not instantiated");
                    },
                    Some(ref mut walk) => {
                        let neighbor_keys_opt = {
                            let mut tx = self.peerdb.tx_begin()
                                .map_err(|_e| net_error::DBError)?;

                            let res = walk.neighbor_handshakes_try_finish(&mut tx, local_peer, chain_view.burn_block_height)?;
                            tx.commit()
                                .map_err(|_e| net_error::DBError)?;
                            res
                        };

                        match neighbor_keys_opt {
                            None => {
                                // nothing to do -- still working 
                                Ok(())
                            },
                            Some(neighbor_keys) => {
                                // finished handshaking.  Proceed to estimate cur_neighbor's in-degree
                                let mut pending_getneighbors = HashMap::new();
                                let now = get_epoch_time_secs();

                                for nk in neighbor_keys {
                                    if !self.is_registered(&nk) {
                                        // not connected to this neighbor -- can't ask for neighbors 
                                        warn!("Not connected to {:?}", &nk);
                                        continue;
                                    }

                                    test_debug!("{:?}: send GetNeighbors to {:?}", &local_peer, &nk);

                                    let msg = self.sign_for_peer(local_peer, chain_view, &nk, StacksMessageType::GetNeighbors)?;
                                    let rh_res = self.send_message(&nk, msg, now + NEIGHBOR_REQUEST_TIMEOUT);
                                    match rh_res {
                                        Ok(rh) => {
                                            pending_getneighbors.insert(nk, rh);
                                        }
                                        Err(e) => {
                                            // failed to begin getneighbors 
                                            debug!("Not connected to {:?}: {:?}", &nk, &e);
                                            continue;
                                        }
                                    }
                                }

                                walk.getneighbors_neighbors_begin(local_peer, pending_getneighbors);
                                Ok(())
                            }
                        }
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Make progress on completing getneighbors requests to all of cur_neighbor's neighbors.  If
    /// we finish, proceed to update our knowledge of these neighbors and take a step in the peer
    /// graph.
    pub fn walk_getneighbors_neighbors_try_finish(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<Option<Neighbor>, net_error> {
        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => {
                        panic!("Invalid neighbor-walk state reached -- cannot finish gathering neighbor's frontier's GetNeighbors replies when the walk state is not instantiated");
                    },
                    Some(ref mut walk) => {
                        let neighbor_opt = {
                            let mut tx = self.peerdb.tx_begin()
                                .map_err(|_e| net_error::DBError)?;
                            
                            let neighbor_opt = walk.getneighbors_neighbors_try_finish(&mut tx, local_peer)?;
                            tx.commit()
                                .map_err(|_e| net_error::DBError)?;

                            neighbor_opt
                        };

                        match neighbor_opt {
                            None => {
                                // not done yet 
                                Ok(None)
                            },
                            Some(_neighbor) => {
                                // finished calculating this neighbor's in/out degree.
                                // walk to the next neighbor.
                                let next_neighbor_opt = walk.step(self.peerdb.conn());
                                let mut ping_handles = HashMap::new();

                                // proceed to ping/handshake neighbors we need to replace
                                for (nk, slot) in walk.replaced_neighbors.iter() {
                                    test_debug!("{:?}: send Handshake to replaceable neighbor {:?}", &local_peer, nk);

                                    let handshake_data = HandshakeData::from_local_peer(local_peer);
                                    let msg = self.sign_for_peer(local_peer, chain_view, nk, StacksMessageType::Handshake(handshake_data))?;
                                    let req_res = self.send_message(nk, msg, get_epoch_time_secs() + NEIGHBOR_REQUEST_TIMEOUT);
                                    match req_res {
                                        Ok(handle) => {
                                            ping_handles.insert((*nk).clone(), handle);
                                        }
                                        Err(e) => {
                                            debug!("Not connected to {:?}: ({:?}", nk, &e);
                                        }
                                    };
                                }

                                walk.ping_existing_neighbors_begin(local_peer, ping_handles);
                                Ok(next_neighbor_opt)
                            }
                        }
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Make progress on completing pings to existing neighbors we'd like to replace.  If we
    /// finish, proceed to update our peer database.
    /// Return the result of the peer walk, and reset the walk state.
    pub fn walk_ping_existing_neighbors_try_finish(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Result<Option<NeighborWalkResult>, net_error> {
        let mut walk = self.walk.take();
        let res = {
            let mut trycatch = |my_walk: &mut Option<NeighborWalk>| {
                match my_walk {
                    None => {
                        panic!("Invalid neighbor-walk state reached -- cannot finish pinging stale neighbors when walk state is not instantiated");
                    },
                    Some(ref mut walk) => {
                        let replaced_opt = {
                            let mut tx = self.peerdb.tx_begin()
                                .map_err(|_e| net_error::DBError)?;

                            let res = walk.ping_existing_neighbors_try_finish(&mut tx, local_peer, self.burnchain.network_id)?;
                            tx.commit()
                                .map_err(|_e| net_error::DBError)?;

                            res
                        };

                        match replaced_opt {
                            None => {
                                // still working
                                Ok(None)
                            },
                            Some(_) => {
                                // finished!
                                // extract the walk result
                                let neighbor_walk_result = {
                                    let mut next_neighbor_opt = walk.next_neighbor.take();
                                    match next_neighbor_opt {
                                        Some(ref mut next_neighbor) => {
                                            test_debug!("Stepped to {:?}", &next_neighbor.addr);
                                            walk.reset(&next_neighbor.clone())
                                        }
                                        None => {
                                            // need to select a random new neighbor 
                                            let next_neighbors = self.get_random_neighbors(1, chain_view.burn_block_height)?;
                                            test_debug!("Did not step to any neighbor; resetting walk to {:?}", &next_neighbors[0].addr);
                                            walk.reset(&next_neighbors[0])
                                        }
                                    }
                                };

                                Ok(Some(neighbor_walk_result))
                            }
                        }
                    }
                }
            };
            trycatch(&mut walk)
        };

        self.walk = walk;
        res
    }

    /// Update the state of our peer graph walk.
    /// If we complete a walk, give back a walk result.
    /// Mask errors by restarting the graph walk.
    pub fn walk_peer_graph(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView) -> Option<NeighborWalkResult> {
        if self.walk.is_none() {
            // time to do a walk yet?
            if self.walk_count > NUM_INITIAL_WALKS && self.walk_deadline > get_epoch_time_secs() {
                // we've done enough walks for an initial mixing,
                // so throttle ourselves down until the walk deadline passes.
                return None;
            }
        }

        let walk_state =
            match self.walk {
                None => {
                    NeighborWalkState::HandshakeBegin
                },
                Some(ref walk) => {
                    walk.state.clone()
                }
            };

        test_debug!("{:?}: {:?}", &local_peer, walk_state);

        let res = match walk_state {
            NeighborWalkState::HandshakeBegin => {
                self.walk_handshake_begin(local_peer, chain_view)
                    .and_then(|_r| Ok(None))
            },
            NeighborWalkState::HandshakeFinish => {
                self.walk_handshake_try_finish(local_peer, chain_view)
                    .and_then(|_r| Ok(None))
            },
            NeighborWalkState::GetNeighborsBegin => {
                self.walk_getneighbors_begin(local_peer, chain_view)
                    .and_then(|_r| Ok(None))
            },
            NeighborWalkState::GetNeighborsFinish => {
                self.walk_getneighbors_try_finish(local_peer, chain_view)
                    .and_then(|_r| Ok(None))
            },
            NeighborWalkState::GetHandshakesFinish => {
                self.walk_neighbor_handshakes_try_finish(local_peer, chain_view)
                    .and_then(|r| Ok(None))
            },
            NeighborWalkState::GetNeighborsNeighborsFinish => {
                self.walk_getneighbors_neighbors_try_finish(local_peer, chain_view)
                    .and_then(|r| Ok(None))
            },
            NeighborWalkState::NeighborsPingFinish => {
                self.walk_ping_existing_neighbors_try_finish(local_peer, chain_view)
            }
            _ => {
                panic!("Reached invalid walk state {:?}", walk_state);
            }
        };

        match res {
            Ok(walk_opt) => {
                // finished a walk.
                self.walk_count += 1;
                self.walk_deadline = self.connection_opts.walk_interval + get_epoch_time_secs();

                // Randomly restart it if we have done enough walks
                let reset = match self.walk {
                    Some(ref walk) => {
                        test_debug!("{:?}: walk has taken {} steps", &local_peer, walk.walk_step_count);
                        if self.walk_count > NUM_INITIAL_WALKS && walk.walk_step_count >= walk.walk_min_duration {
                            let mut rng = OsRng;
                            let sample : f64 = rng.gen();
                            if walk.walk_step_count >= walk.walk_max_duration || sample < walk.walk_reset_prob {
                                true
                            }
                            else {
                                false
                            }
                        }
                        else {
                            false
                        }
                    },
                    None => false
                };

                if reset {
                    test_debug!("{:?}: random walk restart", &local_peer);
                    self.walk = None;
                }
                
                walk_opt
            },
            Err(e) => {
                test_debug!("{:?}: Restarting neighbor with new random neighbors: {:?} => {:?}", &local_peer, walk_state, &e);
                self.walk = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use net::asn::*;
    use net::chat::*;
    use net::db::*;
    use net::test::*;
    use util::hash::*;

    const TEST_IN_OUT_DEGREES : u64 = 0x1;

    #[test]
    fn test_walk_1_neighbor() {
        let mut peer_1_config = TestPeerConfig::from_port(32000);
        let peer_2_config = TestPeerConfig::from_port(32001);

        // peer 1 crawls peer 2
        peer_1_config.add_neighbor(&peer_2_config.to_neighbor());

        let mut peer_1 = TestPeer::new(&peer_1_config);
        let mut peer_2 = TestPeer::new(&peer_2_config);

        for i in 0..10 {
            let unhandled_1 = peer_1.step();
            let unhandled_2 = peer_2.step();

            let walk_1_end_time = match peer_1.network.walk {
                Some(ref w) => {
                    w.walk_end_time
                }
                None => {
                    0
                }
            };

            let walk_2_end_time = match peer_2.network.walk {
                Some(ref w) => {
                    w.walk_end_time
                }
                None => {
                    0
                }
            };

            if walk_1_end_time > 0 {
                test_debug!("Completed walk in {} step(s)", i);
                break;
            }
        }

        // peer 1 contacted peer 2
        let stats_1 = peer_1.network.get_neighbor_stats(&peer_2.to_neighbor().addr).unwrap();
        assert!(stats_1.last_contact_time > 0);
        assert!(stats_1.last_handshake_time > 0);
        assert!(stats_1.last_send_time > 0);
        assert!(stats_1.last_recv_time > 0);
        assert!(stats_1.bytes_rx > 0);
        assert!(stats_1.bytes_tx > 0);

        let neighbor_2 = peer_2.to_neighbor();

        let (frontier_1, walk_result_1) = match peer_1.network.walk {
            Some(ref walk) => {
                (walk.frontier.clone(), walk.result.clone())
            },
            None => {
                panic!("no walk state for peer 1");
            }
        };

        // peer 2 was added to the frontier
        assert_eq!(frontier_1.len(), 1);
        assert!(frontier_1.get(&neighbor_2.addr).is_some());
        assert_eq!(to_hex(&frontier_1.get(&neighbor_2.addr).unwrap().public_key.to_bytes_compressed()), to_hex(&peer_2.get_public_key().to_bytes_compressed()));

        // peer 2 was new 
        assert_eq!(walk_result_1.new_connections.len(), 1);
        assert!(walk_result_1.new_connections.get(&neighbor_2.addr).is_some());

        // nothing broken or replaced
        assert_eq!(walk_result_1.broken_connections.len(), 0);
        assert_eq!(walk_result_1.replaced_neighbors.len(), 0);
    }
    
    #[test]
    fn test_walk_10_neighbors_of_neighbor() {
        // peer 1 has peer 2 as its neighbor.
        // peer 2 has 10 other neighbors.
        // Goal: peer 1 learns about the 10 other neighbors.
        let mut peer_1_config = TestPeerConfig::from_port(32000);
        let mut peer_2_config = TestPeerConfig::from_port(32001);
        let mut peer_2_neighbors = vec![];
        for i in 0..10 {
            let n = TestPeerConfig::from_port(i + 2 + 32000);
            peer_2_config.add_neighbor(&n.to_neighbor());

            let p = TestPeer::new(&n);
            peer_2_neighbors.push(p);
        }

        // peer 1 crawls peer 2
        peer_1_config.add_neighbor(&peer_2_config.to_neighbor());

        let mut peer_1 = TestPeer::new(&peer_1_config);
        let mut peer_2 = TestPeer::new(&peer_2_config);
        
        // next, make peer 1 discover peer 2's neighbors and peer 2's in/out degree
        for i in 0..20 {
            let unhandled_1 = peer_1.step();
            let unhandled_2 = peer_2.step();
            for j in 0..10 {
                let _ = peer_2_neighbors[j].step();
            }
        }
        
        // peer 1 contacted peer 2
        let stats_1 = peer_1.network.get_neighbor_stats(&peer_2.to_neighbor().addr).unwrap();
        assert!(stats_1.last_contact_time > 0);
        assert!(stats_1.last_handshake_time > 0);
        assert!(stats_1.last_send_time > 0);
        assert!(stats_1.last_recv_time > 0);
        assert!(stats_1.bytes_rx > 0);
        assert!(stats_1.bytes_tx > 0);

        // peer 1 handshaked with all of peer 2's neighbors
        let peer_1_dbconn = peer_1.get_peerdb_conn();
        for peer in &peer_2_neighbors {
            let n = peer.to_neighbor();
            let p_opt = PeerDB::get_peer(peer_1_dbconn, n.addr.network_id, &n.addr.addrbytes, n.addr.port).unwrap();
            match p_opt {
                None => {
                    test_debug!("no such peer: {:?}", &n.addr);
                    assert!(false);
                },
                Some(p) => {
                    assert_eq!(p.public_key, n.public_key);
                    assert_eq!(p.expire_block, n.expire_block);
                }
            }
        }
        
        // peer 1 learned that peer 2 has an out-degree of 10 (10 neighbors) and an in-degree of 1 
        let n2 = peer_2.to_neighbor();
        let p2_opt = PeerDB::get_peer(peer_1_dbconn, n2.addr.network_id, &n2.addr.addrbytes, n2.addr.port).unwrap();
        match p2_opt {
            None => {
                test_debug!("no peer 2");
                assert!(false);
            },
            Some(p2) => {
                assert_eq!(p2.out_degree, 11);
                assert_eq!(p2.in_degree, 1);        // just peer 1
            }
        }
    }

    #[test]
    fn test_walk_2_neighbors() {
        let mut peer_1_config = TestPeerConfig::from_port(32000);
        let mut peer_2_config = TestPeerConfig::from_port(32001);

        peer_1_config.whitelisted = -1;
        peer_2_config.whitelisted = -1;

        // peer 1 crawls peer 2, and peer 2 crawls peer 1
        peer_1_config.add_neighbor(&peer_2_config.to_neighbor());
        peer_2_config.add_neighbor(&peer_1_config.to_neighbor());

        let mut peer_1 = TestPeer::new(&peer_1_config);
        let mut peer_2 = TestPeer::new(&peer_2_config);

        for i in 0..20 {
            let unhandled_1 = peer_1.step();
            let unhandled_2 = peer_2.step();

            let walk_1_end_time = match peer_1.network.walk {
                Some(ref w) => {
                    w.walk_end_time
                }
                None => {
                    0
                }
            };

            let walk_2_end_time = match peer_2.network.walk {
                Some(ref w) => {
                    w.walk_end_time
                }
                None => {
                    0
                }
            };

            if walk_1_end_time > 0 || walk_2_end_time > 0 {
                // walks end at the same time
                assert!(walk_1_end_time > 0);
                assert!(walk_2_end_time > 0);
                break;
            }
        }

        // peer 1 contacted peer 2
        let stats_1 = peer_1.network.get_neighbor_stats(&peer_2.to_neighbor().addr).unwrap();
        assert!(stats_1.last_contact_time > 0);
        assert!(stats_1.last_handshake_time > 0);
        assert!(stats_1.last_send_time > 0);
        assert!(stats_1.last_recv_time > 0);
        assert!(stats_1.bytes_rx > 0);
        assert!(stats_1.bytes_tx > 0);
        
        // peer 2 contacted peer 1
        let stats_2 = peer_2.network.get_neighbor_stats(&peer_1.to_neighbor().addr).unwrap();
        assert!(stats_2.last_contact_time > 0);
        assert!(stats_2.last_handshake_time > 0);
        assert!(stats_2.last_send_time > 0);
        assert!(stats_2.last_recv_time > 0);
        assert!(stats_2.bytes_rx > 0);
        assert!(stats_2.bytes_tx > 0);

        let neighbor_1 = peer_1.to_neighbor();
        let neighbor_2 = peer_2.to_neighbor();

        let (frontier_1, walk_result_1) = match peer_1.network.walk {
            Some(ref walk) => {
                (walk.frontier.clone(), walk.result.clone())
            },
            None => {
                panic!("no walk state for peer 1");
            }
        };
        
        let (frontier_2, walk_result_2) = match peer_2.network.walk {
            Some(ref walk) => {
                (walk.frontier.clone(), walk.result.clone())
            },
            None => {
                panic!("no walk state for peer 2");
            }
        };
        
        // peer 1 was added to the frontier of peer 2
        assert_eq!(frontier_1.len(), 1);
        assert!(frontier_1.get(&neighbor_2.addr).is_some());
        assert_eq!(to_hex(&frontier_1.get(&neighbor_2.addr).unwrap().public_key.to_bytes_compressed()), to_hex(&peer_2.get_public_key().to_bytes_compressed()));

        // peer 2 was added to the frontier of peer 1
        assert_eq!(frontier_2.len(), 1);
        assert!(frontier_2.get(&neighbor_1.addr).is_some());
        assert_eq!(to_hex(&frontier_2.get(&neighbor_1.addr).unwrap().public_key.to_bytes_compressed()), to_hex(&peer_1.get_public_key().to_bytes_compressed()));

        // peer 1 was new to peer 2
        assert_eq!(walk_result_2.new_connections.len(), 1);
        assert!(walk_result_2.new_connections.get(&neighbor_1.addr).is_some());

        // peer 2 was new to peer 1
        assert_eq!(walk_result_1.new_connections.len(), 1);
        assert!(walk_result_1.new_connections.get(&neighbor_2.addr).is_some());

        // nothing broken or replaced
        assert_eq!(walk_result_1.broken_connections.len(), 0);
        assert_eq!(walk_result_1.replaced_neighbors.len(), 0);
        assert_eq!(walk_result_2.broken_connections.len(), 0);
        assert_eq!(walk_result_2.replaced_neighbors.len(), 0);
    }

    #[test]
    fn test_walk_2_neighbors_rekey() {
        let mut peer_1_config = TestPeerConfig::from_port(32000);
        let mut peer_2_config = TestPeerConfig::from_port(32001);

        peer_1_config.whitelisted = -1;
        peer_2_config.whitelisted = -1;
        
        let first_block_height = peer_1_config.burnchain.first_block_height + 1;

        // make keys expire soon
        peer_1_config.private_key_expire = first_block_height + 3;
        peer_2_config.private_key_expire = first_block_height + 4;

        peer_1_config.connection_opts.private_key_lifetime = 5;
        peer_2_config.connection_opts.private_key_lifetime = 5;

        // peer 1 crawls peer 2, and peer 2 crawls peer 1
        peer_1_config.add_neighbor(&peer_2_config.to_neighbor());
        peer_2_config.add_neighbor(&peer_1_config.to_neighbor());

        let mut peer_1 = TestPeer::new(&peer_1_config);
        let mut peer_2 = TestPeer::new(&peer_2_config);

        let initial_public_key_1 = peer_1.get_public_key();
        let initial_public_key_2 = peer_2.get_public_key();

        let mut frontier_1_opt = None;
        let mut walk_result_1_opt = None;
        let mut frontier_2_opt = None;
        let mut walk_result_2_opt = None;

        // walk for a bit
        for i in 0..10 {
            for j in 0..5 {
                let unhandled_1 = peer_1.step();
                let unhandled_2 = peer_2.step();

                let walk_1_end_time = match peer_1.network.walk {
                    Some(ref w) => {
                        w.walk_end_time
                    }
                    None => {
                        0
                    }
                };

                let walk_2_end_time = match peer_2.network.walk {
                    Some(ref w) => {
                        w.walk_end_time
                    }
                    None => {
                        0
                    }
                };
                
                match peer_1.network.walk {
                    Some(ref walk) => {
                        frontier_1_opt = Some(walk.frontier.clone());
                        walk_result_1_opt = Some(walk.result.clone());
                    },
                    None => {}
                };
                
                match peer_2.network.walk {
                    Some(ref walk) => {
                        frontier_2_opt = Some(walk.frontier.clone());
                        walk_result_2_opt = Some(walk.result.clone());
                    },
                    None => {}
                };
            }

            let empty_block_1 = peer_1.empty_burnchain_block(i + first_block_height);
            let empty_block_2 = peer_2.empty_burnchain_block(i + first_block_height);

            peer_1.next_burnchain_block(&empty_block_1);
            peer_2.next_burnchain_block(&empty_block_2);
        }

        let frontier_1 = frontier_1_opt.unwrap();
        let frontier_2 = frontier_2_opt.unwrap();
        let walk_result_1 = walk_result_1_opt.unwrap();
        let walk_result_2 = walk_result_2_opt.unwrap();

        // peer 1 contacted peer 2
        let stats_1 = peer_1.network.get_neighbor_stats(&peer_2.to_neighbor().addr).unwrap();
        assert!(stats_1.last_contact_time > 0);
        assert!(stats_1.last_handshake_time > 0);
        assert!(stats_1.last_send_time > 0);
        assert!(stats_1.last_recv_time > 0);
        assert!(stats_1.bytes_rx > 0);
        assert!(stats_1.bytes_tx > 0);
        
        // peer 2 contacted peer 1
        let stats_2 = peer_2.network.get_neighbor_stats(&peer_1.to_neighbor().addr).unwrap();
        assert!(stats_2.last_contact_time > 0);
        assert!(stats_2.last_handshake_time > 0);
        assert!(stats_2.last_send_time > 0);
        assert!(stats_2.last_recv_time > 0);
        assert!(stats_2.bytes_rx > 0);
        assert!(stats_2.bytes_tx > 0);

        let neighbor_1 = peer_1.to_neighbor();
        let neighbor_2 = peer_2.to_neighbor();

        // peer 1 was added to the peer DB of peer 2
        assert!(PeerDB::get_peer(peer_1.network.peerdb.conn(), neighbor_2.addr.network_id, &neighbor_2.addr.addrbytes, neighbor_2.addr.port).unwrap().is_some());
        
        // peer 2 was added to the peer DB of peer 1
        assert!(PeerDB::get_peer(peer_2.network.peerdb.conn(), neighbor_1.addr.network_id, &neighbor_1.addr.addrbytes, neighbor_1.addr.port).unwrap().is_some());
        
        // nothing broken or replaced
        assert_eq!(walk_result_1.broken_connections.len(), 0);
        assert_eq!(walk_result_1.replaced_neighbors.len(), 0);
        assert_eq!(walk_result_2.broken_connections.len(), 0);
        assert_eq!(walk_result_2.replaced_neighbors.len(), 0);

        // new keys
        assert!(peer_1.get_public_key() != initial_public_key_1);
        assert!(peer_2.get_public_key() != initial_public_key_2);
    }
    
    #[test]
    fn test_walk_2_neighbors_different_networks() {
        // peer 1 and 2 try to handshake but never succeed since they have different network IDs
        let mut peer_1_config = TestPeerConfig::from_port(32000);
        let mut peer_2_config = TestPeerConfig::from_port(32001);

        // peer 1 crawls peer 2, and peer 2 crawls peer 1
        peer_1_config.add_neighbor(&peer_2_config.to_neighbor());
        
        // peer 2 thinks peer 1 has the same network ID that it does
        peer_1_config.burnchain.network_id = peer_1_config.burnchain.network_id + 1;
        peer_2_config.add_neighbor(&peer_1_config.to_neighbor());
        peer_1_config.burnchain.network_id = peer_1_config.burnchain.network_id - 1;
        
        // different network IDs
        peer_2_config.burnchain.network_id = peer_1_config.burnchain.network_id + 1;

        let mut peer_1 = TestPeer::new(&peer_1_config);
        let mut peer_2 = TestPeer::new(&peer_2_config);

        let mut frontier_1_opt = None;
        let mut walk_result_1_opt = None;
        let mut frontier_2_opt = None;
        let mut walk_result_2_opt = None;

        for i in 0..20 {
            let unhandled_1 = peer_1.step();
            let unhandled_2 = peer_2.step();

            match peer_1.network.walk {
                Some(ref walk) => {
                    frontier_1_opt = Some(walk.frontier.clone());
                    walk_result_1_opt = Some(walk.result.clone());
                },
                None => {}
            };
            
            match peer_2.network.walk {
                Some(ref walk) => {
                    frontier_2_opt = Some(walk.frontier.clone());
                    walk_result_2_opt = Some(walk.result.clone());
                },
                None => {}
            };

            let walk_1_end_time = match peer_1.network.walk {
                Some(ref w) => {
                    w.walk_end_time
                }
                None => {
                    0
                }
            };

            let walk_2_end_time = match peer_2.network.walk {
                Some(ref w) => {
                    w.walk_end_time
                }
                None => {
                    0
                }
            };

            if walk_1_end_time > 0 || walk_2_end_time > 0 {
                // walks end at the same time
                assert!(walk_1_end_time > 0);
                assert!(walk_2_end_time > 0);
                break;
            }
        }

        // peer 1 did NOT contact peer 2
        let stats_1 = peer_1.network.get_neighbor_stats(&peer_2.to_neighbor().addr);
        assert!(stats_1.is_none());
        
        // peer 2 did NOT contact peer 1
        let stats_2 = peer_2.network.get_neighbor_stats(&peer_1.to_neighbor().addr);
        assert!(stats_2.is_none());

        let neighbor_1 = peer_1.to_neighbor();
        let neighbor_2 = peer_2.to_neighbor();

        let frontier_1 = frontier_1_opt.unwrap();
        let walk_result_1 = walk_result_1_opt.unwrap();
        let frontier_2 = frontier_2_opt.unwrap();
        let walk_result_2 = walk_result_2_opt.unwrap();
        
        // frontiers remain empty
        assert_eq!(frontier_1.len(), 0);
        assert_eq!(frontier_2.len(), 0);
        
        // no new connections
        assert_eq!(walk_result_2.new_connections.len(), 0);
        assert_eq!(walk_result_1.new_connections.len(), 0);

        // nothing broken or replaced
        assert_eq!(walk_result_1.broken_connections.len(), 0);
        assert_eq!(walk_result_1.replaced_neighbors.len(), 0);
        assert_eq!(walk_result_2.broken_connections.len(), 0);
        assert_eq!(walk_result_2.replaced_neighbors.len(), 0);
    }
    
    fn setup_peer_config(i: usize, neighbor_count: usize, peer_count: usize) -> TestPeerConfig {
        let mut conf = TestPeerConfig::from_port(32000 + (i as u16));
        conf.connection_opts.num_neighbors = neighbor_count as u64;
        conf.connection_opts.soft_num_neighbors = neighbor_count as u64;

        conf.connection_opts.num_clients = 256;
        conf.connection_opts.soft_num_clients = 128;

        conf.connection_opts.max_clients_per_host = MAX_NEIGHBORS_DATA_LEN as u64;
        conf.connection_opts.soft_max_clients_per_host = peer_count as u64;

        conf.connection_opts.max_neighbors_per_host = MAX_NEIGHBORS_DATA_LEN as u64;
        conf.connection_opts.soft_max_neighbors_per_host = (neighbor_count/2) as u64;
        conf.connection_opts.soft_max_neighbors_per_org = (neighbor_count/2) as u64;

        conf.connection_opts.walk_interval = 0;

        let j = i as u32;
        conf.burnchain.peer_version = PEER_VERSION | (j << 16) | (j << 8) | j;     // different non-major versions for each peer
        conf
    }

    #[test]
    fn test_walk_ring_whitelist_20() {
        // all initial peers are whitelisted
        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;

        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = -1;      // always whitelisted
            conf.blacklisted = 0;

            peer_configs.push(conf);
        }

        test_walk_ring(&mut peer_configs, NEIGHBOR_COUNT);
    }
    
    #[test]
    fn test_walk_ring_20() {
        // initial peers are neither white- nor blacklisted
        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;

        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = 0;
            conf.blacklisted = 0;

            peer_configs.push(conf);
        }

        test_walk_ring(&mut peer_configs, NEIGHBOR_COUNT);
    }

    #[test]
    fn test_walk_ring_20_org_biased() {
        // one outlier peer has a different org than the others.
        use std::env;

        // ::32000 is in AS 1
        env::set_var("BLOCKSTACK_NEIGHBOR_TEST_32000", "1");

        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;
        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = 0;
            conf.blacklisted = 0;
            if i == 0 {
                conf.asn = 1;
                conf.org = 1;
            }
            else {
                conf.asn = 0;
                conf.org = 0;
            }

            peer_configs.push(conf);
        }

        let peers = test_walk_ring(&mut peer_configs, NEIGHBOR_COUNT);

        // all peers see peer ::32000 as having ASN and Org ID 1
        let peer_0 = peer_configs[0].to_neighbor();
        for i in 1..PEER_COUNT {
            match PeerDB::get_peer(peers[i].network.peerdb.conn(), peer_0.addr.network_id, &peer_0.addr.addrbytes, peer_0.addr.port).unwrap() {
                Some(p) => {
                    assert_eq!(p.asn, 1);
                    assert_eq!(p.org, 1);
                },
                None => {}
            }
        }

        // no peer pruned peer ::32000
        for i in 1..PEER_COUNT {
            match peers[i].network.prune_inbound_counts.get(&peer_0.addr) {
                None => {},
                Some(count) => {
                    assert_eq!(*count, 0);
                }
            }
        }
    }

    fn test_walk_ring(peer_configs: &mut Vec<TestPeerConfig>, neighbor_count: usize) -> Vec<TestPeer> {
        // arrange neighbors into a "ring" topology, where
        // neighbor N is connected to neighbor (N-1)%NUM_NEIGHBORS and (N+1)%NUM_NEIGHBORS.
        let mut peers = vec![];

        let PEER_COUNT = peer_configs.len();
        let NEIGHBOR_COUNT = neighbor_count;

        for i in 0..PEER_COUNT {
            let n = (i + 1) % PEER_COUNT;
            let neighbor = peer_configs[n].to_neighbor();
            peer_configs[i].add_neighbor(&neighbor);
        }
        for i in 1..PEER_COUNT+1 {
            let p = i - 1;
            let neighbor = peer_configs[p].to_neighbor();
            peer_configs[i % PEER_COUNT].add_neighbor(&neighbor);
        }

        for i in 0..PEER_COUNT {
            let p = TestPeer::new(&peer_configs[i]);
            peers.push(p);
        }

        run_topology_test(&mut peers, NEIGHBOR_COUNT, TEST_IN_OUT_DEGREES);

        // no nacks or handshake-rejects
        for i in 0..PEER_COUNT {
            for (_, convo) in peers[i].network.peers.iter() {
                assert!(*convo.stats.msg_rx_counts.get(&StacksMessageID::Nack).unwrap_or(&0) == 0);
                assert!(*convo.stats.msg_rx_counts.get(&StacksMessageID::HandshakeReject).unwrap_or(&0) == 0);
            }
        }

        peers
    }
    
    #[test]
    fn test_walk_line_whitelisted_20() {
        // initial peers are neither white- nor blacklisted
        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;

        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = -1;
            conf.blacklisted = 0;

            peer_configs.push(conf);
        }

        test_walk_line(&mut peer_configs, NEIGHBOR_COUNT, TEST_IN_OUT_DEGREES);
    }
    
    #[test]
    fn test_walk_line_20() {
        // initial peers are neither white- nor blacklisted
        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;

        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = 0;
            conf.blacklisted = 0;

            peer_configs.push(conf);
        }

        test_walk_line(&mut peer_configs, NEIGHBOR_COUNT, TEST_IN_OUT_DEGREES);
    }

    #[test]
    fn test_walk_line_20_org_biased() {
        // one outlier peer has a different org than the others.
        use std::env;

        // ::32000 is in AS 1
        env::set_var("BLOCKSTACK_NEIGHBOR_TEST_32000", "1");

        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 6;     // make this a little bigger to speed this test up
        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = 0;
            conf.blacklisted = 0;
            if i == 0 {
                conf.asn = 1;
                conf.org = 1;
            }
            else {
                conf.asn = 0;
                conf.org = 0;
            }

            peer_configs.push(conf);
        }

        let peers = test_walk_line(&mut peer_configs, NEIGHBOR_COUNT, 0);

        // all peers see peer ::32000 as having ASN and Org ID 1
        let peer_0 = peer_configs[0].to_neighbor();
        for i in 1..PEER_COUNT {
            match PeerDB::get_peer(peers[i].network.peerdb.conn(), peer_0.addr.network_id, &peer_0.addr.addrbytes, peer_0.addr.port).unwrap() {
                Some(p) => {
                    assert_eq!(p.asn, 1);
                    assert_eq!(p.org, 1);
                },
                None => {}
            }
        }

        // no peer pruned peer ::32000
        for i in 1..PEER_COUNT {
            match peers[i].network.prune_inbound_counts.get(&peer_0.addr) {
                None => {},
                Some(count) => {
                    assert_eq!(*count, 0);
                }
            }
        }
    }

    fn test_walk_line(peer_configs: &mut Vec<TestPeerConfig>, neighbor_count: usize, tests: u64) -> Vec<TestPeer> {
        // arrange neighbors into a "line" topology, where
        // neighbor N is connected to neighbor (N-1)%NUM_NEIGHBORS and (N+1)%NUM_NEIGHBORS
        // except for neighbors 0 and 19 (which each only have one neighbor).
        // all initial peers are whitelisted
        let mut peers = vec![];

        let PEER_COUNT = peer_configs.len();
        let NEIGHBOR_COUNT = neighbor_count;
        for i in 0..PEER_COUNT-1 {
            let n = i + 1;
            let neighbor = peer_configs[n].to_neighbor();
            peer_configs[i].add_neighbor(&neighbor);
        }
        for i in 1..PEER_COUNT {
            let p = i - 1;
            let neighbor = peer_configs[p].to_neighbor();
            peer_configs[i].add_neighbor(&neighbor);
        }

        for i in 0..PEER_COUNT {
            let p = TestPeer::new(&peer_configs[i]);
            peers.push(p);
        }

        run_topology_test(&mut peers, NEIGHBOR_COUNT, tests);

        // no nacks or handshake-rejects
        for i in 0..PEER_COUNT {
            for (_, convo) in peers[i].network.peers.iter() {
                assert!(*convo.stats.msg_rx_counts.get(&StacksMessageID::Nack).unwrap_or(&0) == 0);
                assert!(*convo.stats.msg_rx_counts.get(&StacksMessageID::HandshakeReject).unwrap_or(&0) == 0);
            }
        }

        peers
    }

    #[test]
    fn test_walk_star_whitelisted_20() {
        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;
        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = -1;      // always whitelisted
            conf.blacklisted = 0;

            peer_configs.push(conf);
        }

        test_walk_star(&mut peer_configs, NEIGHBOR_COUNT);
    }
    
    #[test]
    fn test_walk_star_20() {
        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;
        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = 0;
            conf.blacklisted = 0;

            peer_configs.push(conf);
        }

        test_walk_star(&mut peer_configs, NEIGHBOR_COUNT);
    }
    
    #[test]
    fn test_walk_star_20_org_biased() {
        // one outlier peer has a different org than the others.
        use std::env;

        // ::32000 is in AS 1
        env::set_var("BLOCKSTACK_NEIGHBOR_TEST_32000", "1");

        let mut peer_configs = vec![];
        let PEER_COUNT : usize = 20;
        let NEIGHBOR_COUNT : usize = 5;
        for i in 0..PEER_COUNT {
            let mut conf = setup_peer_config(i, NEIGHBOR_COUNT, PEER_COUNT);

            conf.whitelisted = 0;
            conf.blacklisted = 0;
            if i == 0 {
                conf.asn = 1;
                conf.org = 1;
            }
            else {
                conf.asn = 0;
                conf.org = 0;
            }

            peer_configs.push(conf);
        }

        let peers = test_walk_star(&mut peer_configs, NEIGHBOR_COUNT);

        // all peers see peer ::32000 as having ASN and Org ID 1
        let peer_0 = peer_configs[0].to_neighbor();
        for i in 1..PEER_COUNT {
            match PeerDB::get_peer(peers[i].network.peerdb.conn(), peer_0.addr.network_id, &peer_0.addr.addrbytes, peer_0.addr.port).unwrap() {
                Some(p) => {
                    assert_eq!(p.asn, 1);
                    assert_eq!(p.org, 1);
                },
                None => {}
            }
        }

        // no peer pruned peer ::32000
        for i in 1..PEER_COUNT {
            match peers[i].network.prune_inbound_counts.get(&peer_0.addr) {
                None => {},
                Some(count) => {
                    assert_eq!(*count, 0);
                }
            }
        }
    }

    fn test_walk_star(peer_configs: &mut Vec<TestPeerConfig>, neighbor_count: usize) -> Vec<TestPeer> {
        // arrange neighbors into a "star" topology, where
        // neighbor 0 is connected to all neighbors N > 0.
        // all initial peers are whitelisted.
        let mut peers = vec![];
        let PEER_COUNT = peer_configs.len();
        let NEIGHBOR_COUNT = neighbor_count;

        for i in 1..PEER_COUNT {
            let neighbor = peer_configs[i].to_neighbor();
            let hub = peer_configs[0].to_neighbor();
            peer_configs[0].add_neighbor(&neighbor);
            peer_configs[i].add_neighbor(&hub);
        }

        for i in 0..PEER_COUNT {
            let p = TestPeer::new(&peer_configs[i]);
            peers.push(p);
        }

        run_topology_test(&mut peers, NEIGHBOR_COUNT, 0);

        // no nacks or handshake-rejects
        for i in 0..PEER_COUNT {
            for (_, convo) in peers[i].network.peers.iter() {
                assert!(*convo.stats.msg_rx_counts.get(&StacksMessageID::Nack).unwrap_or(&0) == 0);
                assert!(*convo.stats.msg_rx_counts.get(&StacksMessageID::HandshakeReject).unwrap_or(&0) == 0);
            }
        }

        peers
    }
    
    fn dump_peers(peers: &Vec<TestPeer>) -> () {
        for i in 0..peers.len() {
            let mut neighbor_index = vec![];
            let mut outbound_neighbor_index = vec![];
            for j in 0..peers.len() {
                let stats_opt = peers[i].network.get_neighbor_stats(&peers[j].to_neighbor().addr);
                match stats_opt {
                    Some(stats) => {
                        neighbor_index.push(j);
                        if stats.outbound {
                            outbound_neighbor_index.push(j);
                        }
                    },
                    None => {}
                }
            }

            let all_neighbors = PeerDB::get_all_peers(peers[i].network.peerdb.conn()).unwrap();
            let num_whitelisted = all_neighbors.iter().fold(0, |mut sum, ref n2| {sum += if n2.whitelisted < 0 { 1 } else { 0 }; sum});
            test_debug!("Neighbor {} (all={}, outbound={}) (total neighbors = {}, total whitelisted = {}): outbound={:?} all={:?}", i, neighbor_index.len(), outbound_neighbor_index.len(), all_neighbors.len(), num_whitelisted, &outbound_neighbor_index, &neighbor_index);
        }
    }

    fn dump_peer_histograms(peers: &Vec<TestPeer>) -> () {
        let mut outbound_hist : HashMap<usize, usize> = HashMap::new();
        let mut inbound_hist : HashMap<usize, usize> = HashMap::new();
        let mut all_hist : HashMap<usize, usize> = HashMap::new();
        for i in 0..peers.len() {
            let mut neighbor_index = vec![];
            let mut inbound_neighbor_index = vec![];
            let mut outbound_neighbor_index = vec![];
            for j in 0..peers.len() {
                let stats_opt = peers[i].network.get_neighbor_stats(&peers[j].to_neighbor().addr);
                match stats_opt {
                    Some(stats) => {
                        neighbor_index.push(j);
                        if stats.outbound {
                            outbound_neighbor_index.push(j);
                        }
                        else {
                            inbound_neighbor_index.push(j);
                        }
                    },
                    None => {}
                }
            }
            for inbound in inbound_neighbor_index.iter() {
                if inbound_hist.contains_key(inbound) {
                    let c = inbound_hist.get(inbound).unwrap().to_owned();
                    inbound_hist.insert(*inbound, c + 1);
                }
                else {
                    inbound_hist.insert(*inbound, 1);
                }
            }
            for outbound in outbound_neighbor_index.iter() {
                if outbound_hist.contains_key(outbound) {
                    let c = outbound_hist.get(outbound).unwrap().to_owned();
                    outbound_hist.insert(*outbound, c + 1);
                }
                else {
                    outbound_hist.insert(*outbound, 1);
                }
            }
            for n in neighbor_index.iter() {
                if all_hist.contains_key(n) {
                    let c = all_hist.get(n).unwrap().to_owned();
                    all_hist.insert(*n, c + 1);
                }
                else {
                    all_hist.insert(*n, 1);
                }
            }
        }
        for i in 0..peers.len() {
            test_debug!("Neighbor {}: #in={} #out={} #all={}", i, inbound_hist.get(&i).unwrap_or(&0), outbound_hist.get(&i).unwrap_or(&0), all_hist.get(&i).unwrap_or(&0));
        }
    }


    fn run_topology_test(peers: &mut Vec<TestPeer>, neighbor_count: usize, test_bits: u64) -> () {
        let PEER_COUNT = peers.len();

        let mut initial_whitelisted : HashMap<NeighborKey, Vec<NeighborKey>> = HashMap::new();
        let mut initial_blacklisted : HashMap<NeighborKey, Vec<NeighborKey>> = HashMap::new();

        for i in 0..PEER_COUNT {
            let nk = peers[i].config.to_neighbor().addr.clone();
            for j in 0..peers[i].config.initial_neighbors.len() {
                let initial = &peers[i].config.initial_neighbors[j];
                if initial.whitelisted < 0 {
                    if !initial_whitelisted.contains_key(&nk) {
                        initial_whitelisted.insert(nk.clone(), vec![]);
                    }
                    initial_whitelisted.get_mut(&nk).unwrap().push(initial.addr.clone());
                }
                if initial.blacklisted < 0 {
                    if !initial_blacklisted.contains_key(&nk) {
                        initial_blacklisted.insert(nk.clone(), vec![]);
                    }
                    initial_blacklisted.get_mut(&nk).unwrap().push(initial.addr.clone());
                }
            }
        }

        for i in 0..PEER_COUNT {
            peers[i].connect_initial().unwrap();
        }

        // go until each neighbor knows about each other neighbor 
        let mut finished = false;
        while !finished {
            finished = true;
            for i in 0..PEER_COUNT {
                let _ = peers[i].step();
                let nk = peers[i].config.to_neighbor().addr;
                
                // whitelisted peers are still connected 
                match initial_whitelisted.get(&nk) {
                    Some(ref peer_list) => {
                        for pnk in peer_list.iter() {
                            if !peers[i].network.events.contains_key(&pnk.clone()) {
                                error!("{:?}: Perma-whitelisted peer {:?} not connected anymore", &nk, &pnk);
                                assert!(false);
                            }
                        }
                    },
                    None => {}
                };

                // blacklisted peers are never connected 
                match initial_blacklisted.get(&nk) {
                    Some(ref peer_list) => {
                        for pnk in peer_list.iter() {
                            if peers[i].network.events.contains_key(&pnk.clone()) {
                                error!("{:?}: Perma-blacklisted peer {:?} connected", &nk, &pnk);
                                assert!(false);
                            }
                        }
                    }
                    None => {}
                };

                // done?
                let all_neighbors = PeerDB::get_all_peers(peers[i].network.peerdb.conn()).unwrap();
                if (all_neighbors.len() as u64) < ((PEER_COUNT - 1) as u64) {
                    let nk = peers[i].config.to_neighbor().addr;
                    test_debug!("waiting for {:?} to fill up its frontier: {}", &nk, all_neighbors.len());
                    finished = false;
                }
            }
            if finished {
                break;
            }

            dump_peers(&peers);
            dump_peer_histograms(&peers);
        }

        dump_peers(&peers);
        dump_peer_histograms(&peers);
    }
}
