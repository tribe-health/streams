// Rust
use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::fmt::{Debug, Formatter, Result as FormatResult};

// 3rd-party
use anyhow::{anyhow, bail, ensure, Result};
use async_trait::async_trait;
use futures::{future, TryStreamExt};
use hashbrown::{HashMap, HashSet};
use rand::{rngs::StdRng, Rng, SeedableRng};

// IOTA

// Streams
use lets::{
    address::{Address, AppAddr, MsgId},
    id::{Identifier, Identity, PermissionDuration, Permissioned, Psk, PskId},
    message::{
        ContentSizeof, ContentUnwrap, ContentWrap, Message as LetsMessage, PreparsedMessage, Topic, TopicHash,
        TransportMessage, HDF, PCF,
    },
    transport::Transport,
};
use spongos::{
    ddml::{
        commands::{sizeof, unwrap, wrap, Absorb, Commit, Mask, Squeeze},
        modifiers::External,
        types::{Mac, Maybe, NBytes, Size, Uint8},
    },
    KeccakF1600, Spongos, SpongosRng,
};

// Local
use crate::{
    api::{
        cursor_store::{CursorStore, InnerCursorStore},
        message::Message,
        messages::Messages,
        send_response::SendResponse,
        user_builder::UserBuilder,
    },
    message::{
        announcement, branch_announcement, keyload, message_types, signed_packet, subscription, tagged_packet,
        unsubscription,
    },
};

const ANN_MESSAGE_NUM: usize = 0; // Announcement is always the first message of authors
const SUB_MESSAGE_NUM: usize = 0; // Subscription is always the first message of subscribers
const INIT_MESSAGE_NUM: usize = 1; // First non-reserved message number

#[derive(PartialEq, Eq, Default)]
struct State {
    /// Users' Identity information, contains keys and logic for signing and verification
    user_id: Option<Identity>,

    /// Address of the stream announcement message
    ///
    /// None if channel is not created or user is not subscribed.
    stream_address: Option<Address>,

    author_identifier: Option<Identifier>,

    /// Users' trusted public keys together with additional sequencing info: (msgid, seq_no) mapped
    /// by branch topic Vec.
    cursor_store: CursorStore,

    /// Mapping of trusted pre shared keys and identifiers
    psk_store: HashMap<PskId, Psk>,

    /// List of Subscribed Identifiers
    subscribers: HashSet<Identifier>,

    spongos_store: HashMap<MsgId, Spongos>,

    base_branch: Topic,

    /// Users' Spongos Storage configuration. If lean, only the announcement message and latest
    /// branch message spongos state is stored. This reduces the overall size of the user
    /// implementation over time. If not lean, all spongos states processed by the user will be
    /// stored.
    lean: bool,

    /// List of known branch topics
    topics: HashSet<Topic>,
}

pub struct User<T> {
    transport: T,

    state: State,
}

impl User<()> {
    pub fn builder() -> UserBuilder<()> {
        UserBuilder::new()
    }
}

impl<T> User<T> {
    pub(crate) fn new<Psks>(user_id: Option<Identity>, psks: Psks, transport: T, lean: bool) -> Self
    where
        Psks: IntoIterator<Item = (PskId, Psk)>,
    {
        let mut psk_store = HashMap::new();
        let subscribers = HashSet::new();

        // Store any pre shared keys
        psks.into_iter().for_each(|(pskid, psk)| {
            psk_store.insert(pskid, psk);
        });

        Self {
            transport,
            state: State {
                user_id,
                cursor_store: CursorStore::new(),
                psk_store,
                subscribers,
                spongos_store: Default::default(),
                stream_address: None,
                author_identifier: None,
                base_branch: Default::default(),
                lean,
                topics: Default::default(),
            },
        }
    }

    /// User's identifier
    pub fn identifier(&self) -> Option<&Identifier> {
        self.identity().ok().map(|id| id.identifier())
    }

    /// User Identity
    fn identity(&self) -> Result<&Identity> {
        self.state
            .user_id
            .as_ref()
            .ok_or_else(|| anyhow!("User does not have a stored identity"))
    }

    pub fn permission(&self, topic: &Topic) -> Option<&Permissioned<Identifier>> {
        self.identifier()
            .and_then(|id| self.state.cursor_store.get_permission(topic, &id))
    }

    /// User's cursor
    fn cursor(&self, topic: &Topic) -> Option<usize> {
        self.identifier()
            .and_then(|id| self.state.cursor_store.get_cursor(topic, &id))
    }

    fn next_cursor(&self, topic: &Topic) -> Result<usize> {
        self.cursor(topic)
            .map(|c| c + 1)
            .ok_or_else(|| anyhow!("User is not a publisher"))
    }

    pub(crate) fn base_branch(&self) -> &Topic {
        &self.state.base_branch
    }

    pub(crate) fn stream_address(&self) -> Option<Address> {
        self.state.stream_address
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn topics(&self) -> impl Iterator<Item = &Topic> + ExactSizeIterator {
        self.state.topics.iter()
    }

    pub(crate) fn topic_by_hash(&self, hash: &TopicHash) -> Option<Topic> {
        self.topics().find(|t| &TopicHash::from(*t) == hash).cloned()
    }

    fn lean(&self) -> bool {
        self.state.lean
    }

    pub(crate) fn cursors(&self) -> impl Iterator<Item = (&Topic, &Permissioned<Identifier>, usize)> + '_ {
        self.state.cursor_store.cursors()
    }

    fn cursors_by_topic(&self, topic: &Topic) -> Result<impl Iterator<Item = (&Permissioned<Identifier>, &usize)>> {
        self.state
            .cursor_store
            .cursors_by_topic(topic)
            .ok_or_else(|| anyhow!("previous topic {} not found in store", topic))
    }

    pub fn subscribers(&self) -> impl Iterator<Item = &Identifier> + Clone + '_ {
        self.state.subscribers.iter()
    }

    fn should_store_cursor(&self, topic: &Topic, subscriber: Permissioned<&Identifier>) -> bool {
        let permission = self.state.cursor_store.get_permission(topic, subscriber.identifier());
        let tracked_and_equal = permission.is_some() && (permission.unwrap().as_ref() == subscriber);
        !subscriber.is_readonly() && !tracked_and_equal
    }

    fn store_spongos(&mut self, msg_address: MsgId, spongos: Spongos, linked_msg_address: MsgId) {
        let is_stream_address = self
            .stream_address()
            .map_or(false, |stream_address| stream_address.relative() == linked_msg_address);
        // Do not remove announcement message from store
        if self.lean() && !is_stream_address {
            self.state.spongos_store.remove(&linked_msg_address);
        }

        self.state.spongos_store.insert(msg_address, spongos);
    }

    pub fn add_subscriber(&mut self, subscriber: Identifier) -> bool {
        self.state.subscribers.insert(subscriber)
    }

    pub fn remove_subscriber(&mut self, id: &Identifier) -> bool {
        self.state.subscribers.remove(id)
    }

    pub fn add_psk(&mut self, psk: Psk) -> bool {
        self.state.psk_store.insert(psk.to_pskid(), psk).is_none()
    }

    pub fn remove_psk(&mut self, pskid: PskId) -> bool {
        self.state.psk_store.remove(&pskid).is_some()
    }

    /// Sets the latest message link for a specified branch. If the branch does not exist, it is
    /// created
    fn set_latest_link(&mut self, topic: Topic, latest_link: MsgId) -> Option<InnerCursorStore> {
        self.state.cursor_store.set_latest_link(topic, latest_link)
    }

    fn get_latest_link(&self, topic: &Topic) -> Option<MsgId> {
        self.state.cursor_store.get_latest_link(topic)
    }

    pub(crate) async fn handle_message(&mut self, address: Address, msg: TransportMessage) -> Result<Message> {
        let preparsed = msg.parse_header().await?;
        match preparsed.header().message_type() {
            message_types::ANNOUNCEMENT => self.handle_announcement(address, preparsed).await,
            message_types::BRANCH_ANNOUNCEMENT => self.handle_branch_announcement(address, preparsed).await,
            message_types::SUBSCRIPTION => self.handle_subscription(address, preparsed).await,
            message_types::UNSUBSCRIPTION => self.handle_unsubscription(address, preparsed).await,
            message_types::KEYLOAD => self.handle_keyload(address, preparsed).await,
            message_types::SIGNED_PACKET => self.handle_signed_packet(address, preparsed).await,
            message_types::TAGGED_PACKET => self.handle_tagged_packet(address, preparsed).await,
            unknown => Err(anyhow!("unexpected message type {}", unknown)),
        }
    }

    /// Bind Subscriber to the channel announced
    /// in the message.
    async fn handle_announcement(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        // Check Topic
        let publisher = preparsed.header().publisher().clone();

        // Unwrap message
        let announcement = announcement::Unwrap::default();
        let (message, spongos) = preparsed.unwrap(announcement).await?;

        let topic = message.payload().content().topic();
        // Insert new branch into store
        self.state.cursor_store.new_branch(topic.clone());
        self.state.topics.insert(topic.clone());

        // When handling an announcement it means that no cursors have been stored, as no topics are
        // known yet. The message must be unwrapped to retrieve the initial topic before storing cursors
        self.state
            .cursor_store
            .insert_cursor(topic, Permissioned::Admin(publisher), INIT_MESSAGE_NUM);

        // Store spongos
        self.state.spongos_store.insert(address.relative(), spongos);

        // Store message content into stores
        let author_id = message.payload().content().author_id().clone();

        // Update branch links
        self.set_latest_link(topic.clone(), address.relative());
        self.state.author_identifier = Some(author_id);
        self.state.base_branch = topic.clone();
        self.state.stream_address = Some(address);

        Ok(Message::from_lets_message(address, message))
    }

    async fn handle_branch_announcement(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        // Retrieve header values
        let prev_topic = self
            .topic_by_hash(preparsed.header().topic_hash())
            .ok_or_else(|| anyhow!("No known topic that matches header topic"))?;
        let publisher = preparsed.header().publisher().clone();
        let cursor = preparsed.header().sequence();

        // From the point of view of cursor tracking, the message exists, regardless of the validity or
        // accessibility to its content. Therefore we must update the cursor of the publisher before
        // handling the message
        let permission = self
            .state
            .cursor_store
            .get_permission(&prev_topic, &publisher)
            .ok_or_else(|| anyhow!("branch announcement received from user that is not stored as a publisher"))?
            .clone();
        self.state.cursor_store.insert_cursor(&prev_topic, permission, cursor);

        // Unwrap message
        let linked_msg_address = preparsed.header().linked_msg_address().ok_or_else(|| {
            anyhow!(
                "branch announcement messages must contain the address of the message they are linked to in the header"
            )
        })?;
        let mut linked_msg_spongos = {
            if let Some(spongos) = self.state.spongos_store.get(&linked_msg_address).copied() {
                // Spongos must be copied because wrapping mutates it
                spongos
            } else {
                return Ok(Message::orphan(address, preparsed));
            }
        };
        let branch_announcement = branch_announcement::Unwrap::new(&mut linked_msg_spongos);
        let (message, spongos) = preparsed.unwrap(branch_announcement).await?;

        let new_topic = message.payload().content().new_topic();
        // Store spongos
        self.store_spongos(address.relative(), spongos, linked_msg_address);
        // Insert new branch into store
        self.state.cursor_store.new_branch(new_topic.clone());
        self.state.topics.insert(new_topic.clone());
        // Collect permissions from previous branch and clone them into new branch
        let prev_permissions = self
            .cursors_by_topic(&prev_topic)?
            .map(|(id, _)| id.clone())
            .collect::<Vec<Permissioned<Identifier>>>();
        for id in prev_permissions {
            self.state.cursor_store.insert_cursor(new_topic, id, INIT_MESSAGE_NUM);
        }

        // Update branch links
        self.set_latest_link(new_topic.clone(), address.relative());

        Ok(Message::from_lets_message(address, message))
    }

    async fn handle_subscription(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        // Cursor is not stored, as cursor is only tracked for subscribers with write permissions

        // Unwrap message
        let linked_msg_address = preparsed.header().linked_msg_address().ok_or_else(|| {
            anyhow!("subscription messages must contain the address of the message they are linked to in the header")
        })?;
        let mut linked_msg_spongos = {
            if let Some(spongos) = self.state.spongos_store.get(&linked_msg_address).copied() {
                // Spongos must be copied because wrapping mutates it
                spongos
            } else {
                return Ok(Message::orphan(address, preparsed));
            }
        };
        let user_ke_sk = &self.identity()?.ke_sk()?;
        let subscription = subscription::Unwrap::new(&mut linked_msg_spongos, user_ke_sk);
        let (message, _spongos) = preparsed.unwrap(subscription).await?;

        // Store spongos
        // Subscription messages are never stored in spongos to maintain consistency about the view of the
        // set of messages of the stream between all the subscribers and across stateless recovers

        // Store message content into stores
        let subscriber_identifier = message.payload().content().subscriber_identifier();
        self.add_subscriber(subscriber_identifier.clone());

        Ok(Message::from_lets_message(address, message))
    }

    async fn handle_unsubscription(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        // Cursor is not stored, as user is unsubscribing

        // Unwrap message
        let linked_msg_address = preparsed.header().linked_msg_address().ok_or_else(|| {
            anyhow!("signed packet messages must contain the address of the message they are linked to in the header")
        })?;
        let mut linked_msg_spongos = {
            if let Some(spongos) = self.state.spongos_store.get(&linked_msg_address) {
                // Spongos must be cloned because wrapping mutates it
                *spongos
            } else {
                return Ok(Message::orphan(address, preparsed));
            }
        };
        let unsubscription = unsubscription::Unwrap::new(&mut linked_msg_spongos);
        let (message, spongos) = preparsed.unwrap(unsubscription).await?;

        // Store spongos
        self.store_spongos(address.relative(), spongos, linked_msg_address);

        // Store message content into stores
        self.remove_subscriber(message.payload().content().subscriber_identifier());

        Ok(Message::from_lets_message(address, message))
    }

    async fn handle_keyload(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        let stream_address = self
            .stream_address()
            .ok_or_else(|| anyhow!("before handling a keyload one must have received a stream announcement first"))?;
        let topic = self
            .topic_by_hash(preparsed.header().topic_hash())
            .ok_or_else(|| anyhow!("No known topic that matches header topic"))?;
        let publisher = preparsed.header().publisher().clone();
        // Confirm keyload came from administrator
        if !self
            .state
            .cursor_store
            .get_permission(&topic, &publisher)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?
            .is_admin()
        {
            return Err(anyhow!("received keyload message from a user without admin privileges"));
        }
        // From the point of view of cursor tracking, the message exists, regardless of the validity or
        // accessibility to its content. Therefore we must update the cursor of the publisher before
        // handling the message
        self.state
            .cursor_store
            .insert_cursor(&topic, Permissioned::Admin(publisher), preparsed.header().sequence());

        // Unwrap message
        // Ok to unwrap since an author identifier is set at the same time as the stream address
        let author_identifier = self.state.author_identifier.as_ref().unwrap();
        let mut announcement_spongos = self
            .state
            .spongos_store
            .get(&stream_address.relative())
            .copied()
            .expect("a subscriber that has received an stream announcement must keep its spongos in store");

        // TODO: Remove Psk from Identity and Identifier, and manage it as a complementary permission
        let keyload = keyload::Unwrap::new(
            &mut announcement_spongos,
            self.state.user_id.as_ref(),
            author_identifier,
            &self.state.psk_store,
        );
        let (message, spongos) = preparsed.unwrap(keyload).await?;

        // Store spongos
        self.state.spongos_store.insert(address.relative(), spongos);

        let subscribers = message.payload().content().subscribers();

        // If a branch admin does not include a user in the keyload, any further messages sent by
        // the user will not be received by the others, so remove them from the publisher pool
        let stored_subscribers: Vec<(Permissioned<Identifier>, usize)> = self
            .cursors_by_topic(&topic)?
            .map(|(perm, cursor)| (perm.clone(), *cursor))
            .collect();

        for (perm, cursor) in stored_subscribers {
            if !(perm.identifier() == author_identifier
                || subscribers.iter().any(|p| p.identifier() == perm.identifier()))
            {
                self.state
                    .cursor_store
                    .insert_cursor(&topic, Permissioned::Read(perm.identifier().clone()), cursor);
            }
        }

        // Store message content into stores
        for subscriber in subscribers {
            if self.should_store_cursor(&topic, subscriber.as_ref()) {
                self.state
                    .cursor_store
                    .insert_cursor(&topic, subscriber.clone(), INIT_MESSAGE_NUM);
            }
        }

        // Have to make message before setting branch links due to immutable borrow in keyload::unwrap
        let final_message = Message::from_lets_message(address, message);
        // Update branch links
        self.set_latest_link(topic, address.relative());
        Ok(final_message)
    }

    async fn handle_signed_packet(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        let topic = self
            .topic_by_hash(preparsed.header().topic_hash())
            .ok_or_else(|| anyhow!("No known topic that matches header topic"))?;
        let publisher = preparsed.header().publisher();
        let permission = self
            .state
            .cursor_store
            .get_permission(&topic, publisher)
            .expect("Publisher does not have a stored cursor on the provided branch")
            .clone();
        // From the point of view of cursor tracking, the message exists, regardless of the validity or
        // accessibility to its content. Therefore we must update the cursor of the publisher before
        // handling the message
        self.state
            .cursor_store
            .insert_cursor(&topic, permission, preparsed.header().sequence());

        // Unwrap message
        let linked_msg_address = preparsed.header().linked_msg_address().ok_or_else(|| {
            anyhow!("signed packet messages must contain the address of the message they are linked to in the header")
        })?;
        let mut linked_msg_spongos = {
            if let Some(spongos) = self.state.spongos_store.get(&linked_msg_address).copied() {
                // Spongos must be copied because wrapping mutates it
                spongos
            } else {
                return Ok(Message::orphan(address, preparsed));
            }
        };
        let signed_packet = signed_packet::Unwrap::new(&mut linked_msg_spongos);
        let (message, spongos) = preparsed.unwrap(signed_packet).await?;

        // Store spongos
        self.store_spongos(address.relative(), spongos, linked_msg_address);

        // Store message content into stores
        self.set_latest_link(topic, address.relative());
        Ok(Message::from_lets_message(address, message))
    }

    async fn handle_tagged_packet(&mut self, address: Address, preparsed: PreparsedMessage) -> Result<Message> {
        let topic = self
            .topic_by_hash(preparsed.header().topic_hash())
            .ok_or_else(|| anyhow!("No known topic that matches header topic"))?;
        let publisher = preparsed.header().publisher();
        let permission = self
            .state
            .cursor_store
            .get_permission(&topic, publisher)
            .expect("Publisher does not have a stored cursor on the provided branch")
            .clone();
        // From the point of view of cursor tracking, the message exists, regardless of the validity or
        // accessibility to its content. Therefore we must update the cursor of the publisher before
        // handling the message
        self.state
            .cursor_store
            .insert_cursor(&topic, permission, preparsed.header().sequence());

        // Unwrap message
        let linked_msg_address = preparsed.header().linked_msg_address().ok_or_else(|| {
            anyhow!("signed packet messages must contain the address of the message they are linked to in the header")
        })?;
        let mut linked_msg_spongos = {
            if let Some(spongos) = self.state.spongos_store.get(&linked_msg_address).copied() {
                // Spongos must be copied because wrapping mutates it
                spongos
            } else {
                return Ok(Message::orphan(address, preparsed));
            }
        };
        let tagged_packet = tagged_packet::Unwrap::new(&mut linked_msg_spongos);
        let (message, spongos) = preparsed.unwrap(tagged_packet).await?;

        // Store spongos
        self.store_spongos(address.relative(), spongos, linked_msg_address);

        // Store message content into stores
        self.set_latest_link(topic, address.relative());

        Ok(Message::from_lets_message(address, message))
    }

    pub async fn backup<P>(&mut self, pwd: P) -> Result<Vec<u8>>
    where
        P: AsRef<[u8]>,
    {
        let mut ctx = sizeof::Context::new();
        ctx.sizeof(&self.state).await?;
        let buf_size = ctx.finalize() + 32; // State + Mac Size

        let mut buf = vec![0; buf_size];

        let mut ctx = wrap::Context::new(&mut buf[..]);
        let key: [u8; 32] = SpongosRng::<KeccakF1600>::new(pwd).gen();
        ctx.absorb(External::new(&NBytes::new(key)))?
            .commit()?
            .squeeze(&Mac::new(32))?;
        ctx.wrap(&mut self.state).await?;
        assert!(
            ctx.stream().is_empty(),
            "Missmatch between buffer size expected by SizeOf ({buf_size}) and actual size of Wrap ({})",
            ctx.stream().len()
        );

        Ok(buf)
    }

    pub async fn restore<B, P>(backup: B, pwd: P, transport: T) -> Result<Self>
    where
        P: AsRef<[u8]>,
        B: AsRef<[u8]>,
    {
        let mut ctx = unwrap::Context::new(backup.as_ref());
        let key: [u8; 32] = SpongosRng::<KeccakF1600>::new(pwd).gen();
        ctx.absorb(External::new(&NBytes::new(key)))?
            .commit()?
            .squeeze(&Mac::new(32))?;
        let mut state = State::default();
        ctx.unwrap(&mut state).await?;
        Ok(User { transport, state })
    }
}

impl<T> User<T>
where
    T: for<'a> Transport<'a, Msg = TransportMessage>,
{
    pub async fn receive_message(&mut self, address: Address) -> Result<Message>
    where
        T: for<'a> Transport<'a, Msg = TransportMessage>,
    {
        let msg = self.transport.recv_message(address).await?;
        self.handle_message(address, msg).await
    }

    /// Start a [`Messages`] stream to traverse the channel messages
    ///
    /// See the documentation in [`Messages`] for more details and examples.
    pub fn messages(&mut self) -> Messages<T> {
        Messages::new(self)
    }

    /// Iteratively fetches all the next messages until internal state has caught up
    ///
    /// If succeeded, returns the number of messages advanced.
    pub async fn sync(&mut self) -> Result<usize> {
        // ignoring the result is sound as Drain::Error is Infallible
        self.messages().try_fold(0, |n, _| future::ok(n + 1)).await
    }

    /// Iteratively fetches all the pending messages from the transport
    ///
    /// Return a vector with all the messages collected. This is a convenience
    /// method around the [`Messages`] stream. Check out its docs for more
    /// advanced usages.
    pub async fn fetch_next_messages(&mut self) -> Result<Vec<Message>> {
        self.messages().try_collect().await
    }
}

impl<T, TSR> User<T>
where
    T: for<'a> Transport<'a, Msg = TransportMessage, SendResponse = TSR>,
{
    /// Prepare channel Announcement message.
    pub async fn create_stream<Top: Into<Topic>>(&mut self, topic: Top) -> Result<SendResponse<TSR>> {
        // Check conditions
        if let Some(appaddr) = self.stream_address() {
            bail!(
                "Cannot create a channel, user is already registered to channel {}",
                appaddr
            );
        }
        // Confirm user has identity
        let identifier = self.identity()?.identifier().clone();
        // Convert topic
        let topic = topic.into();
        // Generate stream address
        let stream_base_address = AppAddr::gen(&identifier, &topic);
        let stream_rel_address = MsgId::gen(stream_base_address, &identifier, &topic, INIT_MESSAGE_NUM);
        let stream_address = Address::new(stream_base_address, stream_rel_address);

        // Prepare HDF and PCF
        let header = HDF::new(message_types::ANNOUNCEMENT, ANN_MESSAGE_NUM, identifier.clone(), &topic)?;
        let content = PCF::new_final_frame().with_content(announcement::Wrap::new(self.identity()?, &topic));

        // Wrap message
        let (transport_msg, spongos) = LetsMessage::new(header, content).wrap().await?;

        // Attempt to send message
        ensure!(
            self.transport.recv_message(stream_address).await.is_err(),
            anyhow!("stream with address '{}' already exists", stream_address)
        );
        let send_response = self.transport.send_message(stream_address, transport_msg).await?;

        // If a message has been sent successfully, insert the base branch into store
        self.state.cursor_store.new_branch(topic.clone());
        self.state.topics.insert(topic.clone());
        // Commit message to stores
        self.state
            .cursor_store
            .insert_cursor(&topic, Permissioned::Admin(identifier.clone()), INIT_MESSAGE_NUM);
        self.state.spongos_store.insert(stream_address.relative(), spongos);

        // Update branch links
        self.set_latest_link(topic.clone(), stream_address.relative());

        // Commit Author Identifier and Stream Address to store
        self.state.stream_address = Some(stream_address);
        self.state.author_identifier = Some(identifier);
        self.state.base_branch = topic;

        Ok(SendResponse::new(stream_address, send_response))
    }

    /// Prepare new branch Announcement message
    pub async fn new_branch(
        &mut self,
        from_topic: impl Into<Topic>,
        to_topic: impl Into<Topic>,
    ) -> Result<SendResponse<TSR>> {
        // Check conditions
        let stream_address = self
            .stream_address()
            .ok_or_else(|| anyhow!("before starting a new branch, the stream must be created"))?;
        // Confirm user has identity
        let identifier = self.identity()?.identifier().clone();
        // Check Topic
        let topic: Topic = to_topic.into();
        let prev_topic: Topic = from_topic.into();
        // Check Permission
        let permission = self
            .state
            .cursor_store
            .get_permission(&prev_topic, &identifier)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?;
        if permission.is_readonly() {
            return Err(anyhow!("user has read only permissions for this branch"));
        }
        let link_to = self
            .get_latest_link(&prev_topic)
            .ok_or_else(|| anyhow!("No latest link found in branch <{}>", prev_topic))?;

        // Update own's cursor
        let user_cursor = self
            .next_cursor(&prev_topic)
            .map_err(|_| anyhow!("No cursor found in base branch"))?;
        let msgid = MsgId::gen(stream_address.base(), &identifier, &prev_topic, user_cursor);
        let address = Address::new(stream_address.base(), msgid);

        // Prepare HDF and PCF
        // Spongos must be copied because wrapping mutates it
        let mut linked_msg_spongos = self
            .state
            .spongos_store
            .get(&link_to)
            .copied()
            .ok_or_else(|| anyhow!("message '{}' not found in spongos store", link_to))?;
        let header = HDF::new(
            message_types::BRANCH_ANNOUNCEMENT,
            user_cursor,
            identifier.clone(),
            &prev_topic,
        )?
        .with_linked_msg_address(link_to);
        let content = PCF::new_final_frame().with_content(branch_announcement::Wrap::new(
            &mut linked_msg_spongos,
            self.identity()?,
            &topic,
        ));

        // Wrap message
        let (transport_msg, spongos) = LetsMessage::new(header, content).wrap().await?;
        let send_response = self.transport.send_message(address, transport_msg).await?;

        // If message has been sent successfully, create the new branch in store
        self.state.cursor_store.new_branch(topic.clone());
        self.state.topics.insert(topic.clone());
        // Commit message to stores and update cursors
        self.state.cursor_store.insert_cursor(
            &prev_topic,
            Permissioned::Admin(identifier.clone()),
            self.next_cursor(&prev_topic)?,
        );
        self.state.spongos_store.insert(address.relative(), spongos);
        // Collect permissions from previous branch and clone them into new branch
        let prev_permissions = self
            .cursors_by_topic(&prev_topic)?
            .map(|(id, _)| id.clone())
            .collect::<Vec<Permissioned<Identifier>>>();
        for id in prev_permissions {
            self.state.cursor_store.insert_cursor(&topic, id, INIT_MESSAGE_NUM);
        }

        // Update branch links
        self.state.cursor_store.set_latest_link(topic, address.relative());
        Ok(SendResponse::new(address, send_response))
    }

    /// Prepare Subscribe message.
    pub async fn subscribe(&mut self) -> Result<SendResponse<TSR>> {
        // Check conditions
        let stream_address = self
            .stream_address()
            .ok_or_else(|| anyhow!("before subscribing one must receive the announcement of a stream first"))?;
        // Confirm user has identity
        let user_id = self.identity()?;
        let identifier = user_id.identifier();
        // Get base branch topic
        let base_branch = &self.state.base_branch;
        // Link message to channel announcement
        let link_to = stream_address.relative();
        let rel_address = MsgId::gen(stream_address.base(), &identifier, base_branch, SUB_MESSAGE_NUM);

        // Prepare HDF and PCF
        // Spongos must be copied because wrapping mutates it
        let mut linked_msg_spongos = self
            .state
            .spongos_store
            .get(&link_to)
            .copied()
            .ok_or_else(|| anyhow!("message '{}' not found in spongos store", link_to))?;
        let unsubscribe_key = StdRng::from_entropy().gen();
        let author_ke_pk = self
            .state
            .author_identifier
            .as_ref()
            .expect("a user that already have an stream address must know the author identifier")
            .ke_pk()
            .await?;
        let content = PCF::new_final_frame().with_content(subscription::Wrap::new(
            &mut linked_msg_spongos,
            unsubscribe_key,
            user_id,
            &author_ke_pk,
        ));
        let header = HDF::new(
            message_types::SUBSCRIPTION,
            SUB_MESSAGE_NUM,
            identifier.clone(),
            base_branch,
        )?
        .with_linked_msg_address(link_to);

        // Wrap message
        let (transport_msg, _spongos) = LetsMessage::new(header, content).wrap().await?;

        // Attempt to send message
        let message_address = Address::new(stream_address.base(), rel_address);
        ensure!(
            self.transport.recv_message(message_address).await.is_err(),
            anyhow!("there's already a message with address '{}'", message_address)
        );
        let send_response = self.transport.send_message(message_address, transport_msg).await?;

        // If message has been sent successfully, commit message to stores
        // - Subscription messages are not stored in the cursor store
        // - Subscription messages are never stored in spongos to maintain consistency about the view of the
        // set of messages of the stream between all the subscribers and across stateless recovers
        Ok(SendResponse::new(message_address, send_response))
    }

    pub async fn unsubscribe(&mut self) -> Result<SendResponse<TSR>> {
        // Check conditions
        let stream_address = self.stream_address().ok_or_else(|| {
            anyhow!("before sending a subscription one must receive the announcement of a stream first")
        })?;
        // Confirm user has identity
        let user_id = self.identity()?;
        let identifier = user_id.identifier().clone();
        // Get base branch topic
        let base_branch = &self.state.base_branch;
        // Link message to channel announcement
        let link_to = self
            .get_latest_link(base_branch)
            .ok_or_else(|| anyhow!("No latest link found in branch <{}>", base_branch))?;

        // Update own's cursor
        let new_cursor = self.next_cursor(base_branch)?;
        let rel_address = MsgId::gen(stream_address.base(), &identifier, base_branch, new_cursor);

        // Prepare HDF and PCF
        // Spongos must be copied because wrapping mutates it
        let mut linked_msg_spongos = self
            .state
            .spongos_store
            .get(&link_to)
            .copied()
            .ok_or_else(|| anyhow!("message '{}' not found in spongos store", link_to))?;
        let content = PCF::new_final_frame().with_content(unsubscription::Wrap::new(&mut linked_msg_spongos, user_id));
        let header = HDF::new(
            message_types::UNSUBSCRIPTION,
            new_cursor,
            identifier.clone(),
            base_branch,
        )?
        .with_linked_msg_address(link_to);

        // Wrap message
        let (transport_msg, spongos) = LetsMessage::new(header, content).wrap().await?;

        // Attempt to send message
        let message_address = Address::new(stream_address.base(), rel_address);
        ensure!(
            self.transport.recv_message(message_address).await.is_err(),
            anyhow!("there's already a message with address '{}'", message_address)
        );
        let send_response = self.transport.send_message(message_address, transport_msg).await?;

        // If message has been sent successfully, commit message to stores
        let permission = Permissioned::Read(identifier);
        self.state
            .cursor_store
            .insert_cursor(base_branch, permission, new_cursor);
        self.store_spongos(rel_address, spongos, link_to);
        Ok(SendResponse::new(message_address, send_response))
    }

    pub async fn send_keyload<'a, Subscribers, Psks, Top>(
        &mut self,
        topic: Top,
        subscribers: Subscribers,
        psk_ids: Psks,
    ) -> Result<SendResponse<TSR>>
    where
        Subscribers: IntoIterator<Item = Permissioned<&'a Identifier>> + Clone,
        Subscribers::IntoIter: ExactSizeIterator,
        Top: Into<Topic>,
        Psks: IntoIterator<Item = PskId>,
    {
        // Check conditions
        let stream_address = self
            .stream_address()
            .ok_or_else(|| anyhow!("before sending a keyload one must create a stream first"))?;
        // Confirm user has identity
        let user_id = self.identity()?;
        let identifier = user_id.identifier().clone();
        // Check Topic
        let topic = topic.into();
        // Check Permission
        let permission = self
            .permission(&topic)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?;
        if !permission.is_admin() {
            return Err(anyhow!("user does not have admin permissions for this branch"));
        }
        // Link message to edge of branch
        let link_to = self
            .get_latest_link(&topic)
            .ok_or_else(|| anyhow!("No latest message found in branch <{}>", topic))?;
        // Update own's cursor
        let new_cursor = self.next_cursor(&topic)?;
        let rel_address = MsgId::gen(stream_address.base(), &identifier, &topic, new_cursor);

        // Prepare HDF and PCF
        // All Keyload messages will attach to stream Announcement message spongos
        let mut announcement_msg_spongos = self
            .state
            .spongos_store
            .get(&stream_address.relative())
            .copied()
            .expect("a subscriber that has received an stream announcement must keep its spongos in store");

        let mut rng = StdRng::from_entropy();
        let encryption_key = rng.gen();
        let nonce = rng.gen();
        let psk_ids_with_psks = psk_ids
            .into_iter()
            .map(|pskid| {
                Ok((
                    pskid,
                    self.state
                        .psk_store
                        .get(&pskid)
                        .ok_or_else(|| anyhow!("unkown psk '{:?}'", pskid))?,
                ))
            })
            .collect::<Result<Vec<(_, _)>>>()?; // collect to handle possible error
        let content = PCF::new_final_frame().with_content(keyload::Wrap::new(
            &mut announcement_msg_spongos,
            subscribers.clone().into_iter().collect::<Vec<_>>(),
            &psk_ids_with_psks,
            encryption_key,
            nonce,
            user_id,
        ));
        let header =
            HDF::new(message_types::KEYLOAD, new_cursor, identifier.clone(), &topic)?.with_linked_msg_address(link_to);

        // Wrap message
        let (transport_msg, spongos) = LetsMessage::new(header, content).wrap().await?;

        // Attempt to send message
        let message_address = Address::new(stream_address.base(), rel_address);
        ensure!(
            self.transport.recv_message(message_address).await.is_err(),
            anyhow!("there's already a message with address '{}'", message_address)
        );
        let send_response = self.transport.send_message(message_address, transport_msg).await?;

        // If message has been sent successfully, commit message to stores
        for subscriber in subscribers {
            if self.should_store_cursor(&topic, subscriber) {
                self.state
                    .cursor_store
                    .insert_cursor(&topic, subscriber.into(), INIT_MESSAGE_NUM);
            }
        }
        self.state
            .cursor_store
            .insert_cursor(&topic, Permissioned::Admin(identifier), new_cursor);
        self.store_spongos(rel_address, spongos, link_to);
        // Update Branch Links
        self.set_latest_link(topic, message_address.relative());
        Ok(SendResponse::new(message_address, send_response))
    }

    pub async fn send_keyload_for_all<Top>(&mut self, topic: Top) -> Result<SendResponse<TSR>>
    where
        Top: Into<Topic> + Clone,
    {
        let topic = topic.into();
        let permission = self
            .permission(&topic)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?;
        if !permission.is_admin() {
            return Err(anyhow!("user does not have admin permissions for this branch"));
        }
        let psks: Vec<PskId> = self.state.psk_store.keys().copied().collect();
        let subscribers: Vec<Permissioned<Identifier>> = self
            .subscribers()
            .map(|s| {
                if s == permission.identifier() {
                    Permissioned::Admin(s.clone())
                } else {
                    Permissioned::Read(s.clone())
                }
            })
            .collect();
        self.send_keyload(
            topic,
            // Alas, must collect to release the &self immutable borrow
            subscribers.iter().map(Permissioned::as_ref),
            psks,
        )
        .await
    }

    pub async fn send_keyload_for_all_rw<Top>(&mut self, topic: Top) -> Result<SendResponse<TSR>>
    where
        Top: Into<Topic> + Clone,
    {
        let topic = topic.into();
        let permission = self
            .permission(&topic)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?;
        if !permission.is_admin() {
            return Err(anyhow!("user does not have admin permissions for this branch"));
        }
        let psks: Vec<PskId> = self.state.psk_store.keys().copied().collect();
        let subscribers: Vec<Permissioned<Identifier>> = self
            .subscribers()
            .map(|s| {
                if s == permission.identifier() {
                    Permissioned::Admin(s.clone())
                } else {
                    Permissioned::ReadWrite(s.clone(), PermissionDuration::Perpetual)
                }
            })
            .collect();
        self.send_keyload(
            topic,
            // Alas, must collect to release the &self immutable borrow
            subscribers.iter().map(Permissioned::as_ref),
            psks,
        )
        .await
    }

    pub async fn send_signed_packet<P, M, Top>(
        &mut self,
        topic: Top,
        public_payload: P,
        masked_payload: M,
    ) -> Result<SendResponse<TSR>>
    where
        M: AsRef<[u8]>,
        P: AsRef<[u8]>,
        Top: Into<Topic>,
    {
        // Check conditions
        let stream_address = self.stream_address().ok_or_else(|| {
            anyhow!("before sending a signed packet one must receive the announcement of a stream first")
        })?;
        let user_id = self.identity()?;
        let identifier = user_id.identifier().clone();
        // Check Topic
        let topic = topic.into();
        // Check Permission
        let permission = self
            .state
            .cursor_store
            .get_permission(&topic, &identifier)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?
            .clone();
        if permission.is_readonly() {
            return Err(anyhow!("user has read only permissions for this branch"));
        }
        // Link message to latest message in branch
        let link_to = self
            .get_latest_link(&topic)
            .ok_or_else(|| anyhow!("No latest link found in branch <{}>", topic))?;
        // Update own's cursor
        let new_cursor = self.next_cursor(&topic)?;
        let rel_address = MsgId::gen(stream_address.base(), &identifier, &topic, new_cursor);

        // Prepare HDF and PCF
        // Spongos must be copied because wrapping mutates it
        let mut linked_msg_spongos = self
            .state
            .spongos_store
            .get(&link_to)
            .copied()
            .ok_or_else(|| anyhow!("message '{}' not found in spongos store", link_to))?;
        let content = PCF::new_final_frame().with_content(signed_packet::Wrap::new(
            &mut linked_msg_spongos,
            self.identity()?,
            public_payload.as_ref(),
            masked_payload.as_ref(),
        ));
        let header = HDF::new(message_types::SIGNED_PACKET, new_cursor, identifier.clone(), &topic)?
            .with_linked_msg_address(link_to);

        // Wrap message
        let (transport_msg, spongos) = LetsMessage::new(header, content).wrap().await?;

        // Attempt to send message
        let message_address = Address::new(stream_address.base(), rel_address);
        ensure!(
            self.transport.recv_message(message_address).await.is_err(),
            anyhow!("there's already a message with address '{}'", message_address)
        );
        let send_response = self.transport.send_message(message_address, transport_msg).await?;

        // If message has been sent successfully, commit message to stores
        self.state
            .cursor_store
            .insert_cursor(&topic, permission.clone(), new_cursor);
        self.store_spongos(rel_address, spongos, link_to);
        // Update Branch Links
        self.set_latest_link(topic, message_address.relative());
        Ok(SendResponse::new(message_address, send_response))
    }

    pub async fn send_tagged_packet<P, M, Top>(
        &mut self,
        topic: Top,
        public_payload: P,
        masked_payload: M,
    ) -> Result<SendResponse<TSR>>
    where
        M: AsRef<[u8]>,
        P: AsRef<[u8]>,
        Top: Into<Topic>,
    {
        // Check conditions
        let stream_address = self.stream_address().ok_or_else(|| {
            anyhow!("before sending a tagged packet one must receive the announcement of a stream first")
        })?;
        let user_id = self.identity()?;
        let identifier = user_id.identifier().clone();
        // Check Topic
        let topic = topic.into();
        // Check Permission
        let permission = self
            .state
            .cursor_store
            .get_permission(&topic, &identifier)
            .ok_or_else(|| anyhow!("user does not have a cursor stored for this branch"))?
            .clone();
        if permission.is_readonly() {
            return Err(anyhow!("user has read only permissions for this branch"));
        }
        // Link message to latest message in branch
        let link_to = self
            .get_latest_link(&topic)
            .ok_or_else(|| anyhow!("No latest link found in branch <{}>", topic))?;

        // Update own's cursor
        let new_cursor = self.next_cursor(&topic)?;
        let rel_address = MsgId::gen(stream_address.base(), &identifier, &topic, new_cursor);

        // Prepare HDF and PCF
        // Spongos must be copied because wrapping mutates it
        let mut linked_msg_spongos = self
            .state
            .spongos_store
            .get(&link_to)
            .copied()
            .ok_or_else(|| anyhow!("message '{}' not found in spongos store", link_to))?;
        let content = PCF::new_final_frame().with_content(tagged_packet::Wrap::new(
            &mut linked_msg_spongos,
            public_payload.as_ref(),
            masked_payload.as_ref(),
        ));
        let header = HDF::new(message_types::TAGGED_PACKET, new_cursor, identifier.clone(), &topic)?
            .with_linked_msg_address(link_to);

        // Wrap message
        let (transport_msg, spongos) = LetsMessage::new(header, content).wrap().await?;

        // Attempt to send message
        let message_address = Address::new(stream_address.base(), rel_address);
        ensure!(
            self.transport.recv_message(message_address).await.is_err(),
            anyhow!("there's already a message with address '{}'", message_address)
        );
        let send_response = self.transport.send_message(message_address, transport_msg).await?;

        // If message has been sent successfully, commit message to stores
        self.state
            .cursor_store
            .insert_cursor(&topic, permission.clone(), new_cursor);
        self.store_spongos(rel_address, spongos, link_to);
        // Update Branch Links
        self.set_latest_link(topic, rel_address);
        Ok(SendResponse::new(message_address, send_response))
    }
}

#[async_trait(?Send)]
impl ContentSizeof<State> for sizeof::Context {
    async fn sizeof(&mut self, user_state: &State) -> Result<&mut Self> {
        self.mask(Maybe::new(user_state.user_id.as_ref()))?
            .mask(Maybe::new(user_state.stream_address.as_ref()))?
            .mask(Maybe::new(user_state.author_identifier.as_ref()))?
            .mask(&user_state.base_branch)?;

        let amount_spongos = user_state.spongos_store.len();
        self.mask(Size::new(amount_spongos))?;
        for (address, spongos) in &user_state.spongos_store {
            self.mask(address)?.mask(spongos)?;
        }

        // Only keep topics that exist in cursor store, any others serve no purpose
        let topics = user_state
            .topics
            .iter()
            .filter(|t| user_state.cursor_store.get_latest_link(*t).is_some());
        let amount_topics = topics.clone().count();
        self.mask(Size::new(amount_topics))?;

        for topic in topics {
            self.mask(topic)?;
            let latest_link = user_state
                .cursor_store
                .get_latest_link(topic)
                .ok_or_else(|| anyhow!("No latest link found in branch <{}>", topic))?;
            self.mask(&latest_link)?;

            let cursors: Vec<(&Permissioned<Identifier>, &usize)> = user_state
                .cursor_store
                .cursors_by_topic(topic)
                .ok_or_else(|| anyhow!("No cursors found with topic <{}>", topic))?
                .collect();
            let amount_cursors = cursors.len();
            self.mask(Size::new(amount_cursors))?;
            for (subscriber, cursor) in cursors {
                self.mask(subscriber)?.mask(Size::new(*cursor))?;
            }
        }

        let subs = &user_state.subscribers;
        let amount_subs = subs.len();
        self.mask(Size::new(amount_subs))?;
        for subscriber in subs {
            self.mask(subscriber)?;
        }

        let psks = user_state.psk_store.iter();
        let amount_psks = psks.len();
        self.mask(Size::new(amount_psks))?;
        for (pskid, psk) in psks {
            self.mask(pskid)?.mask(psk)?;
        }

        let lean = if user_state.lean { 1 } else { 0 };
        self.mask(Uint8::new(lean))?;

        self.commit()?.squeeze(Mac::new(32))?;
        Ok(self)
    }
}

#[async_trait(?Send)]
impl<'a> ContentWrap<State> for wrap::Context<&'a mut [u8]> {
    async fn wrap(&mut self, user_state: &mut State) -> Result<&mut Self> {
        self.mask(Maybe::new(user_state.user_id.as_ref()))?
            .mask(Maybe::new(user_state.stream_address.as_ref()))?
            .mask(Maybe::new(user_state.author_identifier.as_ref()))?
            .mask(&user_state.base_branch)?;

        let amount_spongos = user_state.spongos_store.len();
        self.mask(Size::new(amount_spongos))?;
        for (address, spongos) in &user_state.spongos_store {
            self.mask(address)?.mask(spongos)?;
        }

        // Only keep topics that exist in cursor store, any others serve no purpose
        let topics = user_state
            .topics
            .iter()
            .filter(|t| user_state.cursor_store.get_latest_link(*t).is_some());
        let amount_topics = topics.clone().count();
        self.mask(Size::new(amount_topics))?;

        for topic in topics {
            self.mask(topic)?;
            let latest_link = user_state
                .cursor_store
                .get_latest_link(topic)
                .ok_or_else(|| anyhow!("No latest link found in branch <{}>", topic))?;
            self.mask(&latest_link)?;

            let cursors: Vec<(&Permissioned<Identifier>, &usize)> = user_state
                .cursor_store
                .cursors_by_topic(topic)
                .ok_or_else(|| anyhow!("No curosrs found with topic <{}>", topic))?
                .collect();
            let amount_cursors = cursors.len();
            self.mask(Size::new(amount_cursors))?;
            for (subscriber, cursor) in cursors {
                self.mask(subscriber)?.mask(Size::new(*cursor))?;
            }
        }

        let subs = &user_state.subscribers;
        let amount_subs = subs.len();
        self.mask(Size::new(amount_subs))?;
        for subscriber in subs {
            self.mask(subscriber)?;
        }

        let psks = user_state.psk_store.iter();
        let amount_psks = psks.len();
        self.mask(Size::new(amount_psks))?;
        for (pskid, psk) in psks {
            self.mask(pskid)?.mask(psk)?;
        }

        let lean = if user_state.lean { 1 } else { 0 };
        self.mask(Uint8::new(lean))?;

        self.commit()?.squeeze(Mac::new(32))?;
        Ok(self)
    }
}

#[async_trait(?Send)]
impl<'a> ContentUnwrap<State> for unwrap::Context<&'a [u8]> {
    async fn unwrap(&mut self, user_state: &mut State) -> Result<&mut Self> {
        self.mask(Maybe::new(&mut user_state.user_id))?
            .mask(Maybe::new(&mut user_state.stream_address))?
            .mask(Maybe::new(&mut user_state.author_identifier))?
            .mask(&mut user_state.base_branch)?;

        let mut amount_spongos = Size::default();
        self.mask(&mut amount_spongos)?;
        for _ in 0..amount_spongos.inner() {
            let mut address = MsgId::default();
            let mut spongos = Spongos::default();
            self.mask(&mut address)?.mask(&mut spongos)?;
            user_state.spongos_store.insert(address, spongos);
        }

        let mut amount_topics = Size::default();
        self.mask(&mut amount_topics)?;

        for _ in 0..amount_topics.inner() {
            let mut topic = Topic::default();
            self.mask(&mut topic)?;
            let mut latest_link = MsgId::default();
            self.mask(&mut latest_link)?;

            user_state.topics.insert(topic.clone());
            user_state.cursor_store.set_latest_link(topic.clone(), latest_link);

            let mut amount_cursors = Size::default();
            self.mask(&mut amount_cursors)?;
            for _ in 0..amount_cursors.inner() {
                let mut subscriber = Permissioned::default();
                let mut cursor = Size::default();
                self.mask(&mut subscriber)?.mask(&mut cursor)?;
                user_state
                    .cursor_store
                    .insert_cursor(&topic, subscriber, cursor.inner());
            }
        }

        let mut amount_subs = Size::default();
        self.mask(&mut amount_subs)?;
        for _ in 0..amount_subs.inner() {
            let mut subscriber = Identifier::default();
            self.mask(&mut subscriber)?;
            user_state.subscribers.insert(subscriber);
        }

        let mut amount_psks = Size::default();
        self.mask(&mut amount_psks)?;
        for _ in 0..amount_psks.inner() {
            let mut pskid = PskId::default();
            let mut psk = Psk::default();
            self.mask(&mut pskid)?.mask(&mut psk)?;
            user_state.psk_store.insert(pskid, psk);
        }

        let mut lean = Uint8::new(0);
        self.mask(&mut lean)?;
        user_state.lean = lean.inner() == 1;

        self.commit()?.squeeze(Mac::new(32))?;
        Ok(self)
    }
}

impl<T> Debug for User<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FormatResult {
        write!(
            f,
            "\n* identifier: <{:?}>\n* topic: {}\n{:?}\n* PSKs: \n{}\n* messages:\n{}\n* lean: {}\n",
            self.identifier(),
            self.base_branch(),
            self.state.cursor_store,
            self.state
                .psk_store
                .keys()
                .map(|pskid| format!("\t<{:?}>\n", pskid))
                .collect::<String>(),
            self.state
                .spongos_store
                .keys()
                .map(|key| format!("\t<{}>\n", key))
                .collect::<String>(),
            self.state.lean
        )
    }
}

/// An streams user equality is determined by the equality of its state. The major consequence of
/// this fact is that two users with the same identity but different transport configurations are
/// considered equal
impl<T> PartialEq for User<T> {
    fn eq(&self, other: &Self) -> bool {
        self.state == other.state
    }
}

/// An streams user equality is determined by the equality of its state. The major consequence of
/// this fact is that two users with the same identity but different transport configurations are
/// considered equal
impl<T> Eq for User<T> {}