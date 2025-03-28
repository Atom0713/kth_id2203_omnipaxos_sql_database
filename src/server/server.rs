use crate::{configs::OmniPaxosSQLConfig, database::Database, network::Network};
use chrono::Utc;
use log::*;
use omnipaxos::{
    messages::Message,
    util::{LogEntry, NodeId},
    OmniPaxos, OmniPaxosConfig,
};
use omnipaxos_sql::common::{kv::*, messages::*, utils::Timestamp};
use omnipaxos_storage::memory_storage::MemoryStorage;
use std::{fs::File, io::Write, time::Duration, usize};

type OmniPaxosInstance = OmniPaxos<Command, MemoryStorage<Command>>;
const NETWORK_BATCH_SIZE: usize = 100;
const LEADER_WAIT: Duration = Duration::from_secs(1);
const ELECTION_TIMEOUT: Duration = Duration::from_secs(1);

pub struct OmniPaxosServer {
    id: NodeId,
    database: Database,
    network: Network,
    omnipaxos: OmniPaxosInstance,
    current_decided_idx: usize,
    omnipaxos_msg_buffer: Vec<Message<Command>>,
    config: OmniPaxosSQLConfig,
    peers: Vec<NodeId>,
}

impl OmniPaxosServer {
    pub async fn new(config: OmniPaxosSQLConfig) -> Self {
        // Initialize OmniPaxos instance
        let storage: MemoryStorage<Command> = MemoryStorage::default();
        let omnipaxos_config: OmniPaxosConfig = config.clone().into();
        let omnipaxos_msg_buffer = Vec::with_capacity(omnipaxos_config.server_config.buffer_size);
        let omnipaxos = omnipaxos_config.build(storage).unwrap();

        // Retrieve the DATABASE_URL environment variable
        let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set");

        // Create the database connection
        let database = Database::new(&database_url).await.unwrap();
    
        // Waits for client and server network connections to be established
        let network = Network::new(config.clone(), NETWORK_BATCH_SIZE).await;

        OmniPaxosServer {
            id: config.local.server_id,
            database,
            network,
            omnipaxos,
            current_decided_idx: 0,
            omnipaxos_msg_buffer,
            peers: config.get_peers(config.local.server_id),
            config,
        }
    }

    pub async fn run(&mut self) {
        // Save config to output file
        self.save_output().expect("Failed to write to file");
        let mut client_msg_buf = Vec::with_capacity(NETWORK_BATCH_SIZE);
        let mut cluster_msg_buf = Vec::with_capacity(NETWORK_BATCH_SIZE);
        // We don't use Omnipaxos leader election at first and instead force a specific initial leader
        self.establish_initial_leader(&mut cluster_msg_buf, &mut client_msg_buf)
            .await;
        // Main event loop with leader election
        let mut election_interval = tokio::time::interval(ELECTION_TIMEOUT);
        loop {
            tokio::select! {
                _ = election_interval.tick() => {
                    self.omnipaxos.tick();
                    self.send_outgoing_msgs();
                },
                _ = self.network.cluster_messages.recv_many(&mut cluster_msg_buf, NETWORK_BATCH_SIZE) => {
                    self.handle_cluster_messages(&mut cluster_msg_buf).await;
                },
                _ = self.network.client_messages.recv_many(&mut client_msg_buf, NETWORK_BATCH_SIZE) => {
                    self.handle_client_messages(&mut client_msg_buf).await;
                },
            }
        }
    }

    // Ensures cluster is connected and initial leader is promoted before returning.
    // Once the leader is established it chooses a synchronization point which the
    // followers relay to their clients to begin the experiment.
    async fn establish_initial_leader(
        &mut self,
        cluster_msg_buffer: &mut Vec<(NodeId, ClusterMessage)>,
        client_msg_buffer: &mut Vec<(ClientId, ClientMessage)>,
    ) {
        let mut leader_takeover_interval = tokio::time::interval(LEADER_WAIT);
        loop {
            tokio::select! {
                _ = leader_takeover_interval.tick(), if self.config.cluster.initial_leader == self.id => {
                    if let Some((curr_leader, is_accept_phase)) = self.omnipaxos.get_current_leader(){
                        if curr_leader == self.id && is_accept_phase {
                            info!("{}: Leader fully initialized", self.id);
                            let experiment_sync_start = (Utc::now() + Duration::from_secs(2)).timestamp_millis();
                            self.send_cluster_start_signals(experiment_sync_start);
                            self.send_client_start_signals(experiment_sync_start);
                            break;
                        }
                    }
                    info!("{}: Attempting to take leadership", self.id);
                    self.omnipaxos.try_become_leader();
                    self.send_outgoing_msgs();
                },
                _ = self.network.cluster_messages.recv_many(cluster_msg_buffer, NETWORK_BATCH_SIZE) => {
                    let recv_start = self.handle_cluster_messages(cluster_msg_buffer).await;
                    if recv_start {
                        break;
                    }
                },
                _ = self.network.client_messages.recv_many(client_msg_buffer, NETWORK_BATCH_SIZE) => {
                    self.handle_client_messages(client_msg_buffer).await;
                },
            }
        }
    }

    async fn handle_decided_entries(&mut self) {
        // TODO: Can use a read_raw here to avoid allocation
        let new_decided_idx = self.omnipaxos.get_decided_idx();
        if self.current_decided_idx < new_decided_idx {
            let decided_entries = self
                .omnipaxos
                .read_decided_suffix(self.current_decided_idx)
                .unwrap();
            self.current_decided_idx = new_decided_idx;
            debug!("Decided {new_decided_idx}");
            let decided_commands = decided_entries
                .into_iter()
                .filter_map(|e| match e {
                    LogEntry::Decided(cmd) => Some(cmd),
                    _ => unreachable!(),
                })
                .collect();
            self.update_database_and_respond(decided_commands).await;
        }
    }

    async fn update_database_and_respond(&mut self, commands: Vec<Command>) {
        // TODO: batching responses possible here (batch at handle_cluster_messages)
        for command in commands {
            let read = self.database.handle_command(command.sql_cmd).await;
            if command.coordinator_id == self.id {
                let response = match read {
                    Some(read_result) => ServerMessage::Read(command.id, read_result),
                    None => ServerMessage::Write(command.id),
                };
                self.network.send_to_client(command.client_id, response);
            }
        }
    }

    fn send_outgoing_msgs(&mut self) {
        self.omnipaxos
            .take_outgoing_messages(&mut self.omnipaxos_msg_buffer);
        for msg in self.omnipaxos_msg_buffer.drain(..) {
            let to = msg.get_receiver();
            let cluster_msg = ClusterMessage::OmniPaxosMessage(msg);
            self.network.send_to_cluster(to, cluster_msg);
        }
    }

    async fn read_from_db(&mut self, command_id: usize, sql_command: SQLCommand) -> ServerMessage {
        let read = self.database.handle_command(sql_command.clone()).await;
        match read {
            Some(read_result) => ServerMessage::Read(command_id, read_result),
            None => ServerMessage::Read(command_id, None),
        }
    }

    async fn handle_client_messages(&mut self, messages: &mut Vec<(ClientId, ClientMessage)>) {
        for (from, message) in messages.drain(..) {
            match message {
                ClientMessage::Append(command_id, ref sql_command) => {
                    match sql_command {
                        SQLCommand::Select(ConsistencyLevel::Linearizable, _key, _query) => {
                            self.append_to_log(from, command_id, sql_command.clone())
                        }
                        SQLCommand::Select(ConsistencyLevel::Local, key, _query) => {
                            debug!("{}: Handle local consistency level read key: {key}", self.id);
                            // just call db handle command
                            // send response to client
                            let response = self.read_from_db(command_id, sql_command.clone()).await;
                            self.network.send_to_client(from, response);
                        }
                        SQLCommand::Select(ConsistencyLevel::Leader, key, _query) => {
                            debug!("{}: Handle Leader consistency level read for key: {key}", self.id);
                            // if leader, just call db handle command, 
                            // else, route message to the leader and leader will handle it here
                            if let Some((curr_leader, _is_accept_phase)) = self.omnipaxos.get_current_leader() {
                                if curr_leader == self.id {
                                    let response = self.read_from_db(command_id, sql_command.clone()).await;
                                    self.network.send_to_client(from, response);
                                }
                                else {
                                    // re-route client message to leader
                                    let command_id: usize = key.parse::<usize>().unwrap();
                                    let request = ClusterMessage::LeaderReadMessage(from, self.id, command_id, sql_command.clone());
                                    debug!("{}: Routing request to the leader {curr_leader}", self.id);
                                    self.network.send_to_cluster(curr_leader, request);
                                    
                                }
                            }
                        }
                        _ => {
                            self.append_to_log(from, command_id, sql_command.clone());
                        }
                        
                    }
                }
            }
        }
        self.send_outgoing_msgs();
    }

    async fn handle_cluster_messages(
        &mut self,
        messages: &mut Vec<(NodeId, ClusterMessage)>,
    ) -> bool {
        let mut received_start_signal = false;
        for (from, message) in messages.drain(..) {
            trace!("{}: Received {message:?}", self.id);
            match message {
                ClusterMessage::OmniPaxosMessage(m) => {
                    self.omnipaxos.handle_incoming(m);
                    self.handle_decided_entries().await;
                }
                ClusterMessage::LeaderStartSignal(start_time) => {
                    debug!("Received start message from peer {from}");
                    received_start_signal = true;
                    self.send_client_start_signals(start_time);
                }
                ClusterMessage::LeaderReadMessage(client_id, receiver, command_id, sql_command) => {
                    debug!("{}: Received leader consistency level read message from server {from}", self.id);
                    let response = self.read_from_db(command_id, sql_command.clone()).await;
                    let request = ClusterMessage::LeaderReadMessageResposne(client_id, response);
                    self.network.send_to_cluster(receiver, request);
                }
                ClusterMessage::LeaderReadMessageResposne(client_id, server_response) => {
                    debug!("{}: Received leader consistency level read response from leader.", self.id);
                    self.network.send_to_client(client_id, server_response);
                }
            }
        }
        self.send_outgoing_msgs();
        received_start_signal
    }

    fn append_to_log(&mut self, from: ClientId, command_id: CommandId, sql_command: SQLCommand) {
        let command = Command {
            client_id: from,
            coordinator_id: self.id,
            id: command_id,
            sql_cmd: sql_command,
        };
        self.omnipaxos
            .append(command)
            .expect("Append to Omnipaxos log failed");
    }

    fn send_cluster_start_signals(&mut self, start_time: Timestamp) {
        for peer in &self.peers {
            debug!("Sending start message to peer {peer}");
            let msg = ClusterMessage::LeaderStartSignal(start_time);
            self.network.send_to_cluster(*peer, msg);
        }
    }

    fn send_client_start_signals(&mut self, start_time: Timestamp) {
        for client_id in 1..self.config.local.num_clients as ClientId + 1 {
            debug!("Sending start message to client {client_id}");
            let msg = ServerMessage::StartSignal(start_time);
            self.network.send_to_client(client_id, msg);
        }
    }

    fn save_output(&mut self) -> Result<(), std::io::Error> {
        let config_json = serde_json::to_string_pretty(&self.config)?;
        let mut output_file = File::create(&self.config.local.output_filepath)?;
        output_file.write_all(config_json.as_bytes())?;
        output_file.flush()?;
        Ok(())
    }
}
