use serde::{Deserialize, Serialize};
use tonic::transport::Channel;

use crate::error::{Result, Error};
use crate::proto::raft_server::{ExecutionReply, ExecutionRequest};
use crate::proto::raft_server::{raft_server_client::RaftServerClient, RegistrationRequest, RegistrationReply};
use super::RpcStatus;

/// A Raft-based key-value client.
pub struct KvClient {
    servers: Vec<RaftServerClient<Channel>>,
    session_id: u64,
    sequence_number: u64,
    last_leader: u64,
}

impl KvClient {
    /// Creates a new Raft client.
    pub fn new(servers: Vec<RaftServerClient<Channel>>) -> Self {
        Self {
            servers,
            session_id: 0,
            sequence_number: 1,
            last_leader: 0,
        }
    }

    /// Registers a new session.
    async fn register(&mut self) -> Result<()> {
        loop {
            match self.servers[self.last_leader as usize].register(RegistrationRequest { }).await {
                Ok(response) => {
                    let RegistrationReply { status, session_id, leader_hint } = response.into_inner();
                    self.last_leader = leader_hint;

                    match Self::deserialize::<RpcStatus>(&status)? {
                        RpcStatus::Ok => {
                            self.session_id = session_id;
                            self.sequence_number = 0;
                            return Ok(());
                        },
                        RpcStatus::NotLeader => { continue; },
                        RpcStatus::SessionExpired => {
                            return Err(Error::Internal("Should not get SessionExpired".into()));
                        },
                    }
                },

                // Timeout, retries the request.
                Err(e) => { continue; },
            }
        }
    }

    /// Executes an operation.
    pub async fn execute(&mut self, operation: Vec<u8>) -> Result<Vec<u8>> {
        loop {
            let execution_request = ExecutionRequest {
                session_id: self.session_id,
                sequence_number: self.sequence_number,
                operation: operation.clone(),
            };
            match self.servers[self.last_leader as usize].execute(execution_request).await {
                Ok(reply) => {
                    let ExecutionReply { status, response, leader_hint } = reply.into_inner();
                    self.last_leader = leader_hint;

                    match Self::deserialize::<RpcStatus>(&status)? {
                        RpcStatus::Ok => {
                            self.sequence_number += 1;
                            let result = Self::deserialize::<Vec<u8>>(&response)?;
                            return Ok(result);
                        },
                        RpcStatus::NotLeader => { continue; },
                        RpcStatus::SessionExpired => { self.register().await?; },
                    }
                },

                // Timeout, retries the request.
                Err(e) => { continue; },
            }
        }
    }

    /// Serializes a value for the Raft client.
    fn serialize<V: Serialize>(value: &V) -> Result<Vec<u8>> {
        Ok(bincode::serialize(value)?)
    }

    /// Deserializes a value from the Raft client.
    fn deserialize<'a, V: Deserialize<'a>>(bytes: &'a [u8]) -> Result<V> {
        Ok(bincode::deserialize(bytes)?)
    }
}