extern crate env_logger;
//#[macro_use]
extern crate log;
#[macro_use]
extern crate detach;
#[macro_use]
extern crate serde;

pub mod cache;
pub mod connection_state;
pub mod error;
pub mod wire_message;

use crate::error::*;
use cache::*;
use connection_state::*;
pub use wire_message::{Entropy, SignedEntropy, WireError, WireMessage};

use detach::prelude::*;
use holochain_tracing::Span;
use lib3h::transport::protocol::*;
use lib3h_protocol::{
    data_types::{EntryData, FetchEntryData, GetListData, Opaque, SpaceData, StoreEntryAspectData},
    protocol::*,
    types::SpaceHash,
    uri::Lib3hUri,
    Address,
};
use lib3h_zombie_actor::prelude::*;

use log::*;
use parking_lot::RwLock;
use rand::Rng;
use std::{collections::HashMap, convert::TryFrom};

pub struct Sim2h {
    pub bound_uri: Option<Lib3hUri>,
    connection_states: RwLock<HashMap<Lib3hUri, ConnectionState>>,
    spaces: HashMap<SpaceHash, RwLock<Space>>,
    transport: Detach<TransportActorParentWrapperDyn<Self>>,
    num_ticks: u32,
}

impl Sim2h {
    pub fn new(transport: DynTransportActor, bind_spec: Lib3hUri) -> Self {
        let t = Detach::new(TransportActorParentWrapperDyn::new(transport, "transport_"));

        let mut sim2h = Sim2h {
            bound_uri: None,
            connection_states: RwLock::new(HashMap::new()),
            spaces: HashMap::new(),
            transport: t,
            num_ticks: 0,
        };

        debug!("Trying to bind to {}...", bind_spec);
        let _ = sim2h.transport.request(
            Span::fixme(),
            RequestToChild::Bind { spec: bind_spec },
            Box::new(|me, response| match response {
                GhostCallbackData::Response(Ok(RequestToChildResponse::Bind(bind_result))) => {
                    debug!("Bound as {}", &bind_result.bound_url);
                    me.bound_uri = Some(bind_result.bound_url);
                    Ok(())
                }
                GhostCallbackData::Response(Err(e)) => Err(format!("Bind error: {}", e).into()),
                GhostCallbackData::Timeout(bt) => Err(format!("timeout: {:?}", bt).into()),
                r => Err(format!(
                    "Got unexpected response from transport actor during bind: {:?}",
                    r
                )
                .into()),
            }),
        );
        sim2h
    }

    fn request_authoring_list(
        &mut self,
        uri: Lib3hUri,
        space_address: SpaceHash,
        provider_agent_id: AgentId,
    ) {
        let wire_message =
            WireMessage::Lib3hToClient(Lib3hToClient::HandleGetAuthoringEntryList(GetListData {
                request_id: "".into(),
                space_address,
                provider_agent_id,
            }));
        self.send(uri, &wire_message);
    }

    fn request_gossiping_list(
        &mut self,
        uri: Lib3hUri,
        space_address: SpaceHash,
        provider_agent_id: AgentId,
    ) {
        let wire_message =
            WireMessage::Lib3hToClient(Lib3hToClient::HandleGetGossipingEntryList(GetListData {
                request_id: "".into(),
                space_address,
                provider_agent_id,
            }));
        self.send(uri, &wire_message);
    }

    // adds an agent to a space
    fn join(
        &mut self,
        uri: &Lib3hUri,
        data: &SpaceData,
        pending_messages: PendingMessages,
    ) -> Sim2hResult<()> {
        let _ = self.connection_states.write().insert(
            uri.clone(),
            ConnectionState::Joined(data.space_address.clone(), data.agent_id.clone()),
        );
        if !self.spaces.contains_key(&data.space_address) {
            self.spaces
                .insert(data.space_address.clone(), RwLock::new(Space::new()));
            info!(
                "\n\n+++++++++++++++\nNew Space: {}\n+++++++++++++++\n",
                data.space_address
            );
        }
        self.spaces
            .get(&data.space_address)
            .unwrap()
            .write()
            .join_agent(data.agent_id.clone(), uri.clone());
        info!(
            "Agent {:?} joined space {:?}",
            data.agent_id, data.space_address
        );
        self.request_authoring_list(
            uri.clone(),
            data.space_address.clone(),
            data.agent_id.clone(),
        );
        self.request_gossiping_list(
            uri.clone(),
            data.space_address.clone(),
            data.agent_id.clone(),
        );
        for message in *pending_messages {
            if let Err(err) = self.handle_message(uri, message.clone()) {
                error!(
                    "Error while handling limbo pending message {:?} for {}: {}",
                    message, uri, err
                );
            }
        }
        Ok(())
    }

    // removes an agent from a space
    fn leave(&self, uri: &Lib3hUri, data: &SpaceData) -> Sim2hResult<()> {
        if let Some(ConnectionState::Joined(space_address, agent_id)) = self.get_connection(uri) {
            if (data.agent_id != agent_id) || (data.space_address != space_address) {
                Err(SPACE_MISMATCH_ERR_STR.into())
            } else {
                self.disconnect(uri);
                Ok(())
            }
        } else {
            Err(format!("no joined agent found at {} ", &uri).into())
        }
    }

    // removes a uri from connection and from spaces
    fn disconnect(&self, uri: &Lib3hUri) {
        if let Some(ConnectionState::Joined(space_address, agent_id)) =
            self.connection_states.write().remove(uri)
        {
            self.spaces
                .get(&space_address)
                .unwrap()
                .write()
                .remove_agent(&agent_id);
        }
    }

    // get the connection status of an agent
    fn get_connection(&self, uri: &Lib3hUri) -> Option<ConnectionState> {
        let reader = self.connection_states.read();
        reader.get(uri).map(|ca| (*ca).clone())
    }

    // find out if an agent is in a space or not and return its URI
    fn lookup_joined(&self, space_address: &SpaceHash, agent_id: &AgentId) -> Option<Lib3hUri> {
        self.spaces
            .get(&space_address)?
            .read()
            .agent_id_to_uri(agent_id)
    }

    // handler for incoming connections
    fn handle_incoming_connect(&self, uri: Lib3hUri) -> Sim2hResult<bool> {
        info!("New connection from {:?}", uri);
        if let Some(_old) = self
            .connection_states
            .write()
            .insert(uri.clone(), ConnectionState::new())
        {
            println!("TODO should remove {}", uri); //TODO
        };
        Ok(true)
    }

    // TODO fix with real crypto
    fn validate_signature(
        _space_data: &SpaceData,
        entropy: &Entropy,
        signed_entropy: &SignedEntropy,
    ) -> bool {
        &format!("{}+fake_signature", entropy) == signed_entropy
    }

    fn make_entropy() -> Entropy {
        "fake_entropy".to_string()
    }

    // handler for messages sent to sim2h
    fn handle_message(&mut self, uri: &Lib3hUri, message: WireMessage) -> Sim2hResult<()> {
        let mut agent = self
            .get_connection(uri)
            .ok_or_else(|| format!("no connection for {}", uri))?;
        match agent {
            // if the agent sending the message is in limbo, then the only message
            // allowed is a join message.
            ConnectionState::Limbo(ref mut pending_messages) => {
                if let WireMessage::ClientToLib3h(ClientToLib3h::JoinSpace(data)) = message {
                    // convert the state to HandShaking
                    let entropy = Sim2h::make_entropy();
                    let agent = ConnectionState::Handshaking(
                        data,
                        entropy.clone(),
                        pending_messages.clone(),
                    );
                    let _ = self.connection_states.write().insert(uri.clone(), agent);
                    self.send(uri.clone(), &WireMessage::SignatureChallenge(entropy));
                    Ok(())
                } else {
                    // TODO: maybe have some upper limit on the number of messages
                    // we allow to queue before dropping the connections
                    pending_messages.push(message);
                    let _ = self.connection_states.write().insert(uri.clone(), agent);
                    self.send(
                        uri.clone(),
                        &WireMessage::Err(WireError::MessageWhileInLimbo),
                    );
                    Ok(())
                }
            }
            ConnectionState::Handshaking(space_data, entropy, mut pending_messages) => {
                if let WireMessage::SignatureChallengeResponse(ref signed_entropy) = message {
                    if Sim2h::validate_signature(&space_data, &entropy, signed_entropy) {
                        self.join(uri, &space_data, pending_messages)?;
                    } else {
                        self.disconnect(uri);
                    }
                } else {
                    // any other messages we just add to pending
                    pending_messages.push(message);
                    // let _ = self.connection_states.write().insert(uri.clone(), agent);
                }
                Ok(())
            }

            // if the agent sending the messages has been vetted and is in the space
            // then build a message to be proxied to the correct destination, and forward it
            ConnectionState::Joined(space_address, agent_id) => {
                if let Some((is_request, to_uri, message)) =
                    self.prepare_proxy(uri, &space_address, &agent_id, message)?
                {
                    if is_request {
                        self.send(to_uri, &message);
                        Ok(())
                    } else {
                        unimplemented!()
                    }
                } else {
                    Ok(())
                }
            }
        }
    }

    // process transport and  incoming messages from it
    pub fn process(&mut self) -> Sim2hResult<()> {
        self.num_ticks += 1;
        if self.num_ticks % 2000 == 0 {
            //std::io::stdout().flush()?;
            debug!(".");
            self.num_ticks = 0;
        }
        detach_run!(&mut self.transport, |t| t.process(self)).map_err(|e| format!("{:?}", e))?;
        for mut transport_message in self.transport.drain_messages() {
            match transport_message
                .take_message()
                .expect("GhostMessage must have a message")
            {
                RequestToParent::ReceivedData { uri, payload } => {
                    match WireMessage::try_from(&payload) {
                        Ok(wire_message) =>
                            if let Err(error) = self.handle_message(&uri, wire_message) {
                                error!("Error handling message: {:?}", error);
                            },
                        Err(error) =>
                            error!(
                                "Could not deserialize received payload into WireMessage!\nError: {:?}\nPayload was: {:?}",
                                error,
                                payload
                            )
                    }
                }
                RequestToParent::IncomingConnection { uri } => {
                    if let Err(error) = self.handle_incoming_connect(uri) {
                        error!("Error handling incomming connection: {:?}", error);
                    }
                }
                RequestToParent::ErrorOccured { uri, error } => match error.kind() {
                    lib3h::transport::error::ErrorKind::Disconnect => {
                        debug!("Disconnecting {} after connection reset", uri);
                        self.disconnect(&uri);
                    }
                    _ => error!(
                        "Transport error occurred on connection to {:?}: {:?}",
                        uri, error,
                    ),
                },
            }
        }
        Ok(())
    }

    // given an incoming messages, prepare a proxy message and whether it's an publish or request
    fn prepare_proxy(
        &mut self,
        uri: &Lib3hUri,
        space_address: &SpaceHash,
        agent_id: &AgentId,
        message: WireMessage,
    ) -> Sim2hResult<Option<(bool, Lib3hUri, WireMessage)>> {
        debug!(
            ">>IN>> {} from {}",
            message.message_type(),
            agent_id.to_string()
        );
        match message {
            // First make sure we are not receiving a message in the wrong direction.
            // Panic for now so we can easily spot a mistake.
            // Should maybe break up WireMessage into two different structs so we get the
            // error already when parsing an incoming payload.
            WireMessage::Lib3hToClient(_) | WireMessage::ClientToLib3hResponse(_) =>
                panic!("This is soo wrong. Clients should never send a message that only servers can send."),
            // -- Space -- //
            WireMessage::ClientToLib3h(ClientToLib3h::JoinSpace(_)) => {
                Err("join message should have been processed elsewhere and can't be proxied".into())
            }
            WireMessage::ClientToLib3h(ClientToLib3h::LeaveSpace(data)) => {
                self.leave(uri, &data).map(|_| None)
            }

            // -- Direct Messaging -- //
            // Send a message directly to another agent on the network
            WireMessage::ClientToLib3h(ClientToLib3h::SendDirectMessage(dm_data)) => {
                if (dm_data.from_agent_id != *agent_id) || (dm_data.space_address != *space_address)
                {
                    return Err(SPACE_MISMATCH_ERR_STR.into());
                }
                let to_url = self
                    .lookup_joined(space_address, &dm_data.to_agent_id)
                    .ok_or_else(|| format!("unvalidated proxy agent {}", &dm_data.to_agent_id))?;
                Ok(Some((
                    true,
                    to_url,
                    WireMessage::Lib3hToClient(Lib3hToClient::HandleSendDirectMessage(dm_data)),
                )))
            }
            // Direct message response
            WireMessage::Lib3hToClientResponse(Lib3hToClientResponse::HandleSendDirectMessageResult(
                dm_data,
            )) => {
                if (dm_data.from_agent_id != *agent_id) || (dm_data.space_address != *space_address)
                {
                    return Err(SPACE_MISMATCH_ERR_STR.into());
                }
                let to_url = self
                    .lookup_joined(space_address, &dm_data.to_agent_id)
                    .ok_or_else(|| format!("unvalidated proxy agent {}", &dm_data.to_agent_id))?;
                Ok(Some((
                    true,
                    to_url,
                    WireMessage::Lib3hToClient(Lib3hToClient::SendDirectMessageResult(dm_data)),
                )))
            }
            WireMessage::ClientToLib3h(ClientToLib3h::PublishEntry(data)) => {
                if (data.provider_agent_id != *agent_id) || (data.space_address != *space_address) {
                    return Err(SPACE_MISMATCH_ERR_STR.into());
                }
                self.handle_new_entry_data(data.entry, space_address.clone(), agent_id.clone());
                Ok(None)
            }
            WireMessage::Lib3hToClientResponse(Lib3hToClientResponse::HandleGetAuthoringEntryListResult(list_data)) => {
                debug!("GOT AUTHORING LIST from {}", agent_id);
                if (list_data.provider_agent_id != *agent_id) || (list_data.space_address != *space_address) {
                    return Err(SPACE_MISMATCH_ERR_STR.into());
                }
                let unseen_aspects = AspectList::from(list_data.address_map)
                    .diff(self.spaces
                        .get(space_address)
                        .expect("This function should not get called if we don't have this space")
                        .read()
                        .all_aspects()
                    );
                debug!("UNSEEN ASPECTS:\n{}", unseen_aspects.pretty_string());
                for entry_address in unseen_aspects.entry_addresses() {
                    if let Some(aspect_address_list) = unseen_aspects.per_entry(entry_address) {
                        let wire_message = WireMessage::Lib3hToClient(
                            Lib3hToClient::HandleFetchEntry(FetchEntryData {
                                request_id: "".into(),
                                space_address: space_address.clone(),
                                provider_agent_id: agent_id.clone(),
                                entry_address: entry_address.clone(),
                                aspect_address_list: Some(aspect_address_list.clone())
                            })
                        );
                        self.send(uri.clone(), &wire_message);
                    }
                }
                Ok(None)
            }
            WireMessage::Lib3hToClientResponse(Lib3hToClientResponse::HandleGetGossipingEntryListResult(list_data)) => {
                debug!("GOT GOSSIPING LIST from {}", agent_id);
                if (list_data.provider_agent_id != *agent_id) || (list_data.space_address != *space_address) {
                    return Err(SPACE_MISMATCH_ERR_STR.into());
                }
                let (agents_in_space, aspects_missing_at_node) = {
                    let space = self.spaces
                        .get(space_address)
                        .expect("This function should not get called if we don't have this space")
                        .read();
                    let aspects_missing_at_node = space
                        .all_aspects()
                        .diff(&AspectList::from(list_data.address_map));

                    warn!("MISSING ASPECTS at {}:\n{}", agent_id, aspects_missing_at_node.pretty_string());

                    let agents_in_space = space
                        .all_agents()
                        .keys()
                        .cloned()
                        .collect::<Vec<Address>>();
                    (agents_in_space, aspects_missing_at_node)
                };

                if agents_in_space.len() == 1 {
                    error!("MISSING ASPECTS and no way to get them. Agent is alone in space..");
                } else {
                    let other_agents = agents_in_space
                        .into_iter()
                        .filter(|a| a!=agent_id)
                        .collect::<Vec<_>>();

                    let mut rng = rand::thread_rng();
                    let random_agent_index = rng.gen_range(0, other_agents.len());
                    let random_agent = other_agents
                        .get(random_agent_index)
                        .expect("Random generator must work as documented");

                    debug!("FETCHING missing contents from RANDOM AGENT: {}", random_agent);

                    let maybe_url = self.lookup_joined(space_address, random_agent);
                    if maybe_url.is_none() {
                        error!("Could not find URL for randomly selected agent. This should not happen!");
                        return Ok(None)
                    }
                    let random_url = maybe_url.unwrap();

                    for entry_address in aspects_missing_at_node.entry_addresses() {
                        if let Some(aspect_address_list) = aspects_missing_at_node.per_entry(entry_address) {
                            let wire_message = WireMessage::Lib3hToClient(
                                Lib3hToClient::HandleFetchEntry(FetchEntryData {
                                    request_id: agent_id.clone().into(),
                                    space_address: space_address.clone(),
                                    provider_agent_id: random_agent.clone(),
                                    entry_address: entry_address.clone(),
                                    aspect_address_list: Some(aspect_address_list.clone())
                                })
                            );
                            debug!("SENDING FeTCH with ReQUest ID: {:?}", wire_message);
                            self.send(random_url.clone(), &wire_message);
                        }
                    }
                }
                Ok(None)
            }
            WireMessage::Lib3hToClientResponse(
                Lib3hToClientResponse::HandleFetchEntryResult(fetch_result)) => {
                if (fetch_result.provider_agent_id != *agent_id) || (fetch_result.space_address != *space_address) {
                    return Err(SPACE_MISMATCH_ERR_STR.into());
                }
                debug!("HANDLE FETCH ENTRY RESULT: {:?}", fetch_result);
                if fetch_result.request_id == String::from("") {
                    debug!("Got FetchEntry result form {} without request id - must be from authoring list", agent_id);
                    self.handle_new_entry_data(fetch_result.entry, space_address.clone(), agent_id.clone());
                } else {
                    debug!("Got FetchEntry result with request id {} - this is for gossiping to agent with incomplete data", fetch_result.request_id);
                    let to_agent_id = Address::from(fetch_result.request_id);
                    let maybe_url = self.lookup_joined(space_address, &to_agent_id);;
                    if maybe_url.is_none() {
                        error!("Got FetchEntryResult with request id that is not a known agent id. My hack didn't work?");
                        return Ok(None)
                    }
                    let url = maybe_url.unwrap();
                    for aspect in fetch_result.entry.aspect_list {
                        let store_message = WireMessage::Lib3hToClient(Lib3hToClient::HandleStoreEntryAspect(
                            StoreEntryAspectData {
                                request_id: "".into(),
                                space_address: space_address.clone(),
                                provider_agent_id: agent_id.clone(),
                                entry_address: fetch_result.entry.entry_address.clone(),
                                entry_aspect: aspect,
                            },
                        ));
                        self.send(url.clone(), &store_message);
                    }
                }

                Ok(None)
            }
            _ => {
                warn!("Ignoring unimplemented message: {:?}", message );
                Err(format!("Message not implemented: {:?}", message).into())
            }
        }
    }

    fn handle_new_entry_data(
        &mut self,
        entry_data: EntryData,
        space_address: SpaceHash,
        provider: Address,
    ) {
        let aspect_addresses = entry_data
            .aspect_list
            .iter()
            .cloned()
            .map(|aspect_data| aspect_data.aspect_address)
            .collect::<Vec<_>>();
        let mut map = HashMap::new();
        map.insert(entry_data.entry_address.clone(), aspect_addresses);
        let aspect_list = AspectList::from(map);
        debug!("GOT NEW ASPECTS:\n{}", aspect_list.pretty_string());

        for aspect in entry_data.aspect_list {
            // 1. Add hashes to our global list of all aspects in this space:
            {
                let mut space = self
                    .spaces
                    .get(&space_address)
                    .expect("This function should not get called if we don't have this space")
                    .write();
                space.add_aspect(
                    entry_data.entry_address.clone(),
                    aspect.aspect_address.clone(),
                );
                debug!(
                    "Space {} now knows about these aspects:\n{}",
                    space_address,
                    space.all_aspects().pretty_string()
                );
            }

            // 2. Create store message
            let store_message = WireMessage::Lib3hToClient(Lib3hToClient::HandleStoreEntryAspect(
                StoreEntryAspectData {
                    request_id: "".into(),
                    space_address: space_address.clone(),
                    provider_agent_id: provider.clone(),
                    entry_address: entry_data.entry_address.clone(),
                    entry_aspect: aspect,
                },
            ));
            // 3. Send store message to everybody in this space
            if let Err(e) = self.broadcast(space_address.clone(), &store_message, Some(&provider)) {
                error!("Error during broadcast: {:?}", e);
            }
        }
    }

    fn broadcast(
        &mut self,
        space: SpaceHash,
        msg: &WireMessage,
        except: Option<&AgentId>,
    ) -> Sim2hResult<()> {
        debug!("Broadcast in space: {:?}", space);
        let all_uris = self
            .spaces
            .get(&space)
            .ok_or("No such space")?
            .read()
            .all_agents()
            .clone()
            .into_iter()
            .filter(|(a, _)| {
                if let Some(exception) = except {
                    *a != *exception
                } else {
                    true
                }
            })
            .collect::<HashMap<AgentId, Lib3hUri>>()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for uri in all_uris {
            debug!("Broadcast: Sending to {:?}", uri);
            self.send(uri, msg);
        }
        Ok(())
    }

    fn send(&mut self, uri: Lib3hUri, msg: &WireMessage) {
        debug!(">>OUT>> {} to {}", msg.message_type(), uri);
        let payload: Opaque = msg.clone().into();
        let send_result = self.transport.request(
            Span::fixme(),
            RequestToChild::SendMessage { uri, payload },
            Box::new(|_me, response| match response {
                GhostCallbackData::Response(Ok(RequestToChildResponse::SendMessageSuccess)) => {
                    Ok(())
                }
                GhostCallbackData::Response(Err(e)) => Err(e.into()),
                GhostCallbackData::Timeout(bt) => Err(format!("timeout: {:?}", bt).into()),
                _ => Err("bad response type".into()),
            }),
        );

        if let Err(e) = send_result {
            error!("GhostError during broadcast send: {:?}", e)
        }
        debug!("sent.");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lib3h::transport::memory_mock::{
        ghost_transport_memory::*, memory_server::get_memory_verse,
    };
    use lib3h_protocol::data_types::*;

    // for this to actually show log entries you also have to run the tests like this:
    // RUST_LOG=lib3h=debug cargo test -- --nocapture
    pub fn enable_logging_for_test(enable: bool) {
        // wait a bit because of non monotonic clock,
        // otherwise we could get negative substraction panics
        // TODO #211
        std::thread::sleep(std::time::Duration::from_millis(10));
        if std::env::var("RUST_LOG").is_err() {
            std::env::set_var("RUST_LOG", "debug");
        }
        let _ = env_logger::builder()
            .default_format_timestamp(false)
            .default_format_module_path(false)
            .is_test(enable)
            .try_init();
    }

    fn make_test_agent() -> AgentId {
        "fake_agent_id".into()
    }

    fn make_test_space_data() -> SpaceData {
        SpaceData {
            request_id: "".into(),
            space_address: "fake_space_address".into(),
            agent_id: make_test_agent(),
        }
    }

    fn make_test_space_data_with_agent(agent_id: AgentId) -> SpaceData {
        SpaceData {
            request_id: "".into(),
            space_address: "fake_space_address".into(),
            agent_id,
        }
    }

    fn make_test_join_message() -> WireMessage {
        make_test_join_message_with_space_data(make_test_space_data())
    }

    fn make_test_join_message_with_space_data(space_data: SpaceData) -> WireMessage {
        WireMessage::ClientToLib3h(ClientToLib3h::JoinSpace(space_data))
    }

    fn make_test_leave_message() -> WireMessage {
        WireMessage::ClientToLib3h(ClientToLib3h::LeaveSpace(make_test_space_data()))
    }

    fn make_test_dm_data_with(from: AgentId, to: AgentId, content: &str) -> DirectMessageData {
        DirectMessageData {
            request_id: "".into(),
            space_address: "fake_space_address".into(),
            from_agent_id: from,
            to_agent_id: to,
            content: content.into(),
        }
    }

    fn make_test_dm_data() -> DirectMessageData {
        make_test_dm_data_with(make_test_agent(), "fake_to_agent_id".into(), "foo")
    }

    fn make_test_dm_message() -> WireMessage {
        make_test_dm_message_with(make_test_dm_data())
    }

    fn make_test_dm_message_with(data: DirectMessageData) -> WireMessage {
        WireMessage::ClientToLib3h(ClientToLib3h::SendDirectMessage(data))
    }

    fn make_test_dm_message_response_with(data: DirectMessageData) -> WireMessage {
        WireMessage::Lib3hToClientResponse(Lib3hToClientResponse::HandleSendDirectMessageResult(
            data,
        ))
    }

    fn make_test_err_message() -> WireMessage {
        WireMessage::Err("fake_error".into())
    }

    fn make_test_sim2h_nonet() -> Sim2h {
        let transport = Box::new(GhostTransportMemory::new("null".into(), "nullnet".into()));
        Sim2h::new(transport, Lib3hUri::with_undefined())
    }

    fn make_test_sim2h_memnet(netname: &str) -> Sim2h {
        let transport_id = "test_transport".into();
        let transport = Box::new(GhostTransportMemory::new(transport_id, netname));
        Sim2h::new(transport, Lib3hUri::with_undefined())
    }

    #[test]
    pub fn test_constructor() {
        let mut sim2h = make_test_sim2h_nonet();
        {
            let reader = sim2h.connection_states.read();
            assert_eq!(reader.len(), 0);
        }
        let result = sim2h.process();
        assert_eq!(result, Ok(()));
        assert_eq!(
            "Some(Lib3hUri(\"mem://addr_1/\"))",
            format!("{:?}", sim2h.bound_uri)
        );
    }

    #[test]
    pub fn test_incomming_connection() {
        let sim2h = make_test_sim2h_nonet();

        // incoming connections get added to the map in limbo
        let uri = Lib3hUri::with_memory("addr_1");
        let result = sim2h.handle_incoming_connect(uri.clone());
        assert_eq!(result, Ok(true));

        let result = sim2h.get_connection(&uri).clone();
        assert_eq!("Some(Limbo([]))", format!("{:?}", result));

        // pretend the agent has joined the space
        let _ = sim2h.connection_states.write().insert(
            uri.clone(),
            ConnectionState::Joined("fake_agent".into(), "fake_space".into()),
        );
        // if we get a second incoming connection, the state should be reset.
        let result = sim2h.handle_incoming_connect(uri.clone());
        assert_eq!(result, Ok(true));
        let result = sim2h.get_connection(&uri).clone();
        assert_eq!("Some(Limbo([]))", format!("{:?}", result));
    }

    #[test]
    pub fn test_join() {
        let mut sim2h = make_test_sim2h_nonet();
        let uri = Lib3hUri::with_memory("addr_1");

        let data = make_test_space_data();
        let result = sim2h.join(&uri, &data, Box::new(Vec::new()));
        assert_eq!(result, Ok(()));
        assert_eq!(
            sim2h.lookup_joined(&data.space_address, &data.agent_id),
            Some(uri.clone())
        );
        let result = sim2h.get_connection(&uri).clone();
        assert_eq!(
            "Some(Joined(SpaceHash(HashString(\"fake_space_address\")), HashString(\"fake_agent_id\")))",
            format!("{:?}", result)
        );
    }

    #[test]
    pub fn test_leave() {
        let mut sim2h = make_test_sim2h_nonet();
        let uri = Lib3hUri::with_memory("addr_1");
        let mut data = make_test_space_data();

        // leaving a space not joined should produce an error
        let result = sim2h.leave(&uri, &data);
        assert_eq!(
            result,
            Err(format!("no joined agent found at {} ", &uri).into())
        );
        let _result = sim2h.handle_incoming_connect(uri.clone());
        let result = sim2h.leave(&uri, &data);
        assert_eq!(
            result,
            Err(format!("no joined agent found at {} ", &uri).into())
        );

        let _result = sim2h.join(&uri, &data, Box::new(Vec::new()));

        // a leave on behalf of someone else should fail
        data.agent_id = "someone_else_agent_id".into();
        let result = sim2h.leave(&uri, &data);
        assert_eq!(result, Err(SPACE_MISMATCH_ERR_STR.into()));

        // a valid leave should work
        data.agent_id = make_test_agent();
        let result = sim2h.leave(&uri, &data);
        assert_eq!(result, Ok(()));
        let result = sim2h.get_connection(&uri).clone();
        assert_eq!(result, None);
        assert_eq!(
            sim2h.lookup_joined(&data.space_address, &data.agent_id),
            None
        );
    }

    #[test]
    pub fn test_prepare_proxy() {
        let mut sim2h = make_test_sim2h_nonet();

        let uri = Lib3hUri::with_memory("addr_1");
        let _ = sim2h.handle_incoming_connect(uri.clone());
        let _ = sim2h.join(&uri, &make_test_space_data(), Box::new(Vec::new()));
        let message = make_test_join_message();
        let data = make_test_space_data();

        // you can't proxy a join message
        let result = sim2h.prepare_proxy(&uri, &data.space_address, &data.agent_id, message);
        assert!(result.is_err());

        // you can't proxy for someone else, i.e. the message contents must match the
        // space joined
        let message = make_test_dm_message();
        let result = sim2h.prepare_proxy(
            &uri,
            &data.space_address,
            &"fake_other_agent".into(),
            message,
        );
        assert_eq!(Err("space/agent id mismatch".into()), result);

        // you can't proxy to someone not in the space
        let message = make_test_dm_message();
        let result =
            sim2h.prepare_proxy(&uri, &data.space_address, &data.agent_id, message.clone());
        assert_eq!(
            Err("unvalidated proxy agent fake_to_agent_id".into()),
            result,
        );

        // proxy a dm message
        // first we have to setup the to agent in the space
        let to_agent_data = make_test_space_data_with_agent("fake_to_agent_id".into());
        let to_uri = Lib3hUri::with_memory("addr_2");
        let _ = sim2h.handle_incoming_connect(to_uri.clone());
        let _ = sim2h.join(&to_uri, &to_agent_data, Box::new(Vec::new()));

        let result = sim2h.prepare_proxy(&uri, &data.space_address, &data.agent_id, message);
        assert_eq!(
            "Ok(Some((true, Lib3hUri(\"mem://addr_2/\"), Lib3hToClient(HandleSendDirectMessage(DirectMessageData { space_address: SpaceHash(HashString(\"fake_space_address\")), request_id: \"\", to_agent_id: HashString(\"fake_to_agent_id\"), from_agent_id: HashString(\"fake_agent_id\"), content: \"foo\" })))))",
            format!("{:?}", result)
        );

        // proxy a dm message response
        // for this test we just pretend the same agent set up above is making a response
        let message = make_test_dm_message_response_with(make_test_dm_data());
        let result = sim2h.prepare_proxy(&uri, &data.space_address, &data.agent_id, message);
        assert_eq!(
            "Ok(Some((true, Lib3hUri(\"mem://addr_2/\"), Lib3hToClient(SendDirectMessageResult(DirectMessageData { space_address: SpaceHash(HashString(\"fake_space_address\")), request_id: \"\", to_agent_id: HashString(\"fake_to_agent_id\"), from_agent_id: HashString(\"fake_agent_id\"), content: \"foo\" })))))",
                format!("{:?}", result)
        );

        // proxy a leave space message should remove the agent from the space
        let message = make_test_leave_message();
        let result = sim2h.prepare_proxy(&uri, &data.space_address, &data.agent_id, message);
        assert_eq!("Ok(None)", format!("{:?}", result));
        let result = sim2h.get_connection(&uri).clone();
        assert_eq!(result, None);
    }

    fn make_test_handshake_response_message(entropy: Entropy) -> WireMessage {
        WireMessage::SignatureChallengeResponse(format!("{}+fake_signature", entropy))
    }

    #[test]
    pub fn test_message() {
        let netname = "test_message";
        let mut sim2h = make_test_sim2h_memnet(netname);
        let network = {
            let mut verse = get_memory_verse();
            verse.get_network(netname)
        };
        let uri = network.lock().unwrap().bind();

        // a message from an unconnected agent should return an error
        let result = sim2h.handle_message(&uri, make_test_err_message());
        assert_eq!(result, Err(format!("no connection for {}", &uri).into()));

        // a non-join message from an unvalidated but connected agent should queue the message
        let _result = sim2h.handle_incoming_connect(uri.clone());
        let result = sim2h.handle_message(&uri, make_test_err_message());
        assert_eq!(result, Ok(()));
        assert_eq!(
            "Some(Limbo([Err(Other(\"\\\"fake_error\\\"\"))]))",
            format!("{:?}", sim2h.get_connection(&uri))
        );

        // a valid join message from a connected agent should update its connection status to handshaking
        let result = sim2h.handle_message(&uri, make_test_join_message());
        assert_eq!(result, Ok(()));
        let result = sim2h.get_connection(&uri).clone();
        let entropy = match result {
            Some(ConnectionState::Handshaking(space_data, entropy, pending_messages)) => {
                assert_eq!(space_data, make_test_space_data());
                assert_eq!(
                    "[Err(Other(\"\\\"fake_error\\\"\"))]",
                    format!("{:?}", pending_messages)
                );
                entropy
            }
            _ => {
                assert!(false);
                "".into()
            }
        };

        // a valid Handshake challenge should then upgrade the connection status to joined
        let handshake_challenge = make_test_handshake_response_message(entropy);
        let result = sim2h.handle_message(&uri, handshake_challenge);
        assert_eq!(result, Ok(()));
        let result = sim2h.get_connection(&uri).clone();
        assert_eq!(
            "Some(Joined(SpaceHash(HashString(\"fake_space_address\")), HashString(\"fake_agent_id\")))",
            format!("{:?}", result)
        );

        // dm
        // first we have to setup the to agent on the in-memory-network and in the space
        let to_uri = network.lock().unwrap().bind();
        let _ = sim2h.handle_incoming_connect(to_uri.clone());
        let to_agent_data = make_test_space_data_with_agent("fake_to_agent_id".into());
        let _ = sim2h.join(&to_uri, &to_agent_data, Box::new(Vec::new()));

        // then we can make a message and handle it.
        let message = make_test_dm_message();
        let result = sim2h.handle_message(&uri, message);
        assert_eq!(result, Ok(()));

        // which should result in showing up in the to_uri's inbox in the in-memory netowrk
        let result = sim2h.process();
        assert_eq!(result, Ok(()));
        let mut reader = network.lock().unwrap();
        let server = reader
            .get_server(&to_uri)
            .expect("there should be a server for to_uri");
        if let Ok((did_work, events)) = server.process() {
            assert!(did_work);
            let dm = &events[3];
            assert_eq!(
                "ReceivedData(Lib3hUri(\"mem://addr_3/\"), \"{\\\"Lib3hToClient\\\":{\\\"HandleSendDirectMessage\\\":{\\\"space_address\\\":\\\"fake_space_address\\\",\\\"request_id\\\":\\\"\\\",\\\"to_agent_id\\\":\\\"fake_to_agent_id\\\",\\\"from_agent_id\\\":\\\"fake_agent_id\\\",\\\"content\\\":\\\"Zm9v\\\"}}}\")",
                format!("{:?}", dm))
        } else {
            assert!(false)
        }
    }

    // creates an agent uri and sends a join request for it to sim2h
    // and simulates the handshake upgrade
    fn test_setup_agent(
        sim2h: &mut Sim2h,
        netname: &str,
        agent_name: &str,
    ) -> (Lib3hUri, SpaceData) {
        let sim2h_uri = sim2h.bound_uri.clone().expect("should have bound");
        let network = {
            let mut verse = get_memory_verse();
            verse.get_network(netname)
        };
        let agent_uri = network.lock().unwrap().bind();

        // connect to sim2h with join messages
        let space_data = make_test_space_data_with_agent(agent_name.into());
        let join: Opaque = make_test_join_message_with_space_data(space_data.clone()).into();
        {
            let mut net = network.lock().unwrap();
            let server = net
                .get_server(&sim2h_uri)
                .expect("there should be a server for to_uri");
            server.request_connect(&agent_uri).expect("can connect");
            let result = server.post(&agent_uri, &join.to_vec());
            assert_eq!(result, Ok(()));
        }
        let _result = sim2h.process();
        let _ = sim2h.join(&agent_uri, &space_data, Box::new(Vec::new()));

        (agent_uri, space_data)
    }

    #[test]
    pub fn test_end_to_end() {
        enable_logging_for_test(true);
        let netname = "test_end_to_end";
        let mut sim2h = make_test_sim2h_memnet(netname);
        let _result = sim2h.process();
        let sim2h_uri = sim2h.bound_uri.clone().expect("should have bound");

        // set up two other agents on the memory-network
        let (agent1_uri, space_data1) = test_setup_agent(&mut sim2h, netname, "agent1");
        let (agent2_uri, space_data2) = test_setup_agent(&mut sim2h, netname, "agent2");

        assert_eq!(
            sim2h.lookup_joined(&space_data1.space_address, &space_data1.agent_id),
            Some(agent1_uri.clone())
        );
        assert_eq!(
            sim2h.lookup_joined(&space_data2.space_address, &space_data2.agent_id),
            Some(agent2_uri.clone())
        );

        // now send a direct message from agent1 through sim2h which should arrive at agent2
        let data = make_test_dm_data_with(
            space_data1.agent_id,
            space_data2.agent_id,
            "come here watson",
        );

        let network = {
            let mut verse = get_memory_verse();
            verse.get_network(netname)
        };
        let message: Opaque = make_test_dm_message_with(data).into();
        {
            let mut net = network.lock().unwrap();
            let server = net
                .get_server(&sim2h_uri)
                .expect("there should be a server for to_uri");
            let result = server.post(&agent1_uri, &message.to_vec());
            assert_eq!(result, Ok(()));
        }
        let _result = sim2h.process();
        let _result = sim2h.process();
        {
            let mut net = network.lock().unwrap();
            let server = net
                .get_server(&agent2_uri)
                .expect("there should be a server for to_uri");
            if let Ok((did_work, events)) = server.process() {
                assert!(did_work);
                let dm = &events[4];
                assert_eq!(
                   "ReceivedData(Lib3hUri(\"mem://addr_1/\"), \"{\\\"Lib3hToClient\\\":{\\\"HandleSendDirectMessage\\\":{\\\"space_address\\\":\\\"fake_space_address\\\",\\\"request_id\\\":\\\"\\\",\\\"to_agent_id\\\":\\\"agent2\\\",\\\"from_agent_id\\\":\\\"agent1\\\",\\\"content\\\":\\\"Y29tZSBoZXJlIHdhdHNvbg==\\\"}}}\")",
                    format!("{:?}", dm))
            } else {
                assert!(false)
            }
        }
    }

    #[test]
    pub fn test_disconnect_and_reconnect() {
        enable_logging_for_test(true);
        let netname = "test_disconnect_and_reconnect";
        let mut sim2h = make_test_sim2h_memnet(netname);
        let _result = sim2h.process();
        let sim2h_uri = sim2h.bound_uri.clone().expect("should have bound");
        let (agent_uri, data) = test_setup_agent(&mut sim2h, netname, "agent");
        let _result = sim2h.process();

        assert_eq!(
            sim2h.lookup_joined(&data.space_address, &data.agent_id),
            Some(agent_uri.clone())
        );

        let network = {
            let mut verse = get_memory_verse();
            verse.get_network(netname)
        };
        {
            let mut net = network.lock().unwrap();
            let server = net
                .get_server(&sim2h_uri)
                .expect("there should be a server for sim2h_uri");
            server.request_close(&agent_uri).expect("can disconnect");
        }
        let _result = sim2h.process();

        assert_eq!(
            sim2h.lookup_joined(&data.space_address, &data.agent_id),
            None
        );

        // connect again and send a dm message
        {
            let mut net = network.lock().unwrap();
            let server = net
                .get_server(&sim2h_uri)
                .expect("there should be a server for sim2h_uri");
            server.request_connect(&agent_uri).expect("can connect");

            let data =
                make_test_dm_data_with(data.agent_id.clone(), data.agent_id, "come here watson");
            let message: Opaque = make_test_dm_message_with(data).into();
            let result = server.post(&agent_uri, &message.to_vec());
            assert_eq!(result, Ok(()));
        }
        let _result = sim2h.process();
        let _result = sim2h.process();
        {
            let mut net = network.lock().unwrap();
            let server = net
                .get_server(&agent_uri)
                .expect("there should be a server for agent_uri");
            if let Ok((did_work, events)) = server.process() {
                assert!(did_work);
                let dm = &events[4];
                assert_eq!(
                    "ReceivedData(Lib3hUri(\"mem://addr_1/\"), \"{\\\"Err\\\":\\\"MessageWhileInLimbo\\\"}\")",
                    format!("{:?}",dm))
            } else {
                assert!(false)
            }
        }
    }
}
