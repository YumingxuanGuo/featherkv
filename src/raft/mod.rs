#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]

mod log;
mod node;
mod state;

pub use node::Node;
pub use self::log::Log;

use crate::error::{Result, Error};
use crate::proto::raft::{RequestVoteArgs, RequestVoteReply, AppendEntriesArgs, AppendEntriesReply};
use crate::proto::raft::raft_service_client::RaftServiceClient;
use crate::server::serialize;
use crate::storage::log::LogDemo;

use std::collections::HashMap;
use futures::Future;
use futures::stream::FuturesUnordered;
use rand::Rng;
use tokio::sync::mpsc;
use tonic::{Response, Status};
use tonic::transport::Channel;

/// The interval between leader heartbeats, in ticks.
const HEARTBEAT_INTERVAL: u64 = 1;
/// The minimum election timeout, in ticks.
const ELECTION_TIMEOUT_MIN: u64 = 8 * HEARTBEAT_INTERVAL;
/// The maximum election timeout, in ticks.
const ELECTION_TIMEOUT_MAX: u64 = 15 * HEARTBEAT_INTERVAL;

/// The local Raft node state machine.
pub enum Role {
    Follower {
        /// The leader, or None if just initialized.
        leader: Option<u64>,
        /// The number of ticks since the last message from the leader.
        leader_seen_ticks: u64,
        /// The timeout before triggering an election.
        leader_seen_timeout: u64,
    },
    Candidate {
        /// Ticks elapsed since election start.
        election_ticks: u64,
        /// Election timeout, in ticks.
        election_timeout: u64,
        /// Votes received (including ourself).
        votes: u64,
    },
    Leader {
        /// Number of ticks since last heartbeat.
        heartbeat_ticks: u64,
        /// The next index to replicate to a peer. // TODO: Hashmap
        next_index: Vec<u64>,
        /// The last index known to be replicated on a peer.
        match_index: Vec<u64>,
        ///
        work_txs: HashMap<u64, mpsc::UnboundedSender<u64>>,
    },
}

impl Role {
    fn init_follower() -> Role {
        Role::Follower {
            leader: None,
            leader_seen_ticks: 0,
            leader_seen_timeout: rand::thread_rng().gen_range(
                ELECTION_TIMEOUT_MIN..ELECTION_TIMEOUT_MAX
            ),
        }
    }

    fn init_candidate() -> Role {
        Role::Candidate {
            election_ticks: 0,
            election_timeout: rand::thread_rng().gen_range(
                ELECTION_TIMEOUT_MIN..ELECTION_TIMEOUT_MAX
            ),
            votes: 1,
        }
    }

    fn init_leader(num_peers: usize, last_index: u64, work_txs: HashMap<u64, mpsc::UnboundedSender<u64>>) -> Role {
        Role::Leader {
            heartbeat_ticks: 0,
            next_index: vec![last_index + 1; num_peers],
            match_index: vec![0; num_peers],
            work_txs,
        }
    }
}

/// A single Raft node.
pub struct Raft {
    peers: Vec<RaftServiceClient<Channel>>,
    // persister

    me: u64,

    /// Persistent state on all servers:
    current_term: u64,
    voted_for: Option<u64>,
    log: Log,

    /// Volatile state on all servers:
    commit_index: u64,
    last_applied: u64,

    /// Volatile state as different roles:
    role: Role,
}

impl Raft {
    /// The service or tester wants to create a Raft server. The ports
    /// of all the Raft servers (including this one) are in `peers`. This
    /// server's port is `peers[me]`. All the servers' peers arrays
    /// have the same order. `Persister` is a place for this server to
    /// save its persistent state, and also initially holds the most
    /// recent saved state, if any. `Apply_ch` is a channel on which the
    /// tester or service expects Raft to send `ApplyMsg` messages.
    /// This method must return quickly.
    /// TODO: improve the function signature
    pub fn new(
        // me: u64,
        // log: Log,
        // peers: Vec<RaftClient>,
        // persister: Box<dyn Persister>,
        // apply_ch: UnboundedSender<ApplyMsg>,
    ) -> Result<Raft> {
        let raft = Raft {
            peers: vec![],
            // persister,
            me: 0,

            current_term: 0,
            voted_for: None,
            log: Log::new(Box::new(LogDemo::new()))?,

            commit_index: 0,
            last_applied: 0,

            role: Role::init_follower(),
        };

        Ok(raft)
    }

    pub fn is_leader(&self) -> bool {
        match self.role {
            Role::Leader { .. } => true,
            _ => false,
        }
    }

    /// Saves Raft's persistent state to stable storage,
    /// where it can later be retrieved after a crash and restart.
    fn persist(&mut self) {
        // Your code here (2C).
        // Example:
        // labcodec::encode(&self.xxx, &mut data).unwrap();
        // labcodec::encode(&self.yyy, &mut data).unwrap();
        // self.persister.save_raft_state(data);
    }

    /// Restores previously persisted state.
    fn restore(&mut self, data: &[u8]) {
        if data.is_empty() {
            // bootstrap without any state?
        }
        // Your code here (2C).
        // Example:
        // match labcodec::decode(data) {
        //     Ok(o) => {
        //         self.xxx = o.xxx;
        //         self.yyy = o.yyy;
        //     }
        //     Err(e) => {
        //         panic!("{:?}", e);
        //     }
        // }
    }

    fn start(&mut self, command: Option<Vec<u8>>) -> Result<(u64, u64)> {
        let index = self.log.last_index + 1;
        let term = self.current_term;
        self.log.append(term, command)?;
        for id in 0..self.peers.len() as u64 {
            if id == self.me {
                continue;
            }
            if let Role::Leader { ref work_txs, .. } = self.role {
                let tx = work_txs.get(&id).unwrap();
                tx.send(index)?;
            } else {
                return Err(Error::Internal(format!("{} is not leader", self.me)));
            }
        }
        Ok((index, term))
    }
}

/// State transition functions.
impl Raft {
    fn quorum(&self) -> u64 {
        self.peers.len() as u64 / 2 + 1
    }

    pub fn become_follower(&mut self, term: u64, leader_id: Option<u64>) {
        self.current_term = term;
        self.voted_for = None;
        self.role = Role::init_follower();
        self.persist();
    }

    pub fn become_candidate(&mut self) {
        self.current_term += 1;
        self.role = Role::init_candidate();
        self.voted_for = Some(self.me);
        self.persist();
    }

    pub fn become_leader(&mut self, work_txs: HashMap<u64, mpsc::UnboundedSender<u64>>) {
        self.role = Role::init_leader(
            self.peers.len(), 
            self.log.last_index,
            work_txs,
        );
        self.persist();
    }

    /// Solicits votes from other nodes.
    pub fn solicit_votes(&self) -> 
        FuturesUnordered<impl Future<Output = core::result::Result<Response<RequestVoteReply>, Status>>> {
        let mut futures = FuturesUnordered::new();
        for i in 0..self.peers.len() {
            if i as u64 == self.me {
                continue;
            }
            let mut client = self.peers[i].clone();
            let args = RequestVoteArgs {
                term: self.current_term,
                candidate_id: self.me,
                last_log_index: 0,
                last_log_term: 0,
            };
            futures.push(async move {
                client.request_vote(args).await
            });
        }
        futures
    }

    /// Sends heartbeats to other nodes.
    pub fn send_heartbeats(&self) {
        for i in 0..self.peers.len() {
            if i as u64 == self.me {
                continue;
            }
            let mut client = self.peers[i].clone();
            let args = AppendEntriesArgs {
                term: self.current_term,
                leader_id: self.me,
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![],
                leader_commit: self.commit_index,
            };
            tokio::spawn(async move {
                client.append_entries(args).await
            });
        }
    }
}