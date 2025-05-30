use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::error::Error;
use std::mem;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hashbrown::HashSet;
use relay_base_schema::project::ProjectKey;
use relay_config::Config;
use tokio::time::{timeout, Instant};

use crate::envelope::Envelope;
use crate::envelope::Item;
use crate::services::buffer::common::ProjectKeyPair;
use crate::services::buffer::envelope_stack::sqlite::SqliteEnvelopeStackError;
use crate::services::buffer::envelope_stack::EnvelopeStack;
use crate::services::buffer::envelope_store::sqlite::SqliteEnvelopeStoreError;
use crate::services::buffer::stack_provider::memory::MemoryStackProvider;
use crate::services::buffer::stack_provider::sqlite::SqliteStackProvider;
use crate::services::buffer::stack_provider::{StackCreationType, StackProvider};
use crate::statsd::{RelayGauges, RelayHistograms, RelayTimers};
use crate::utils::MemoryChecker;

/// Polymorphic envelope buffering interface.
///
/// The underlying buffer can either be disk-based or memory-based,
/// depending on the given configuration.
///
/// NOTE: This is implemented as an enum because a trait object with async methods would not be
/// object safe.
#[derive(Debug)]
#[allow(private_interfaces)]
pub enum PolymorphicEnvelopeBuffer {
    /// An enveloper buffer that uses in-memory envelopes stacks.
    InMemory(EnvelopeBuffer<MemoryStackProvider>),
    /// An enveloper buffer that uses sqlite envelopes stacks.
    Sqlite(EnvelopeBuffer<SqliteStackProvider>),
}

impl PolymorphicEnvelopeBuffer {
    /// Returns true if the implementation stores all envelopes in RAM.
    pub fn is_memory(&self) -> bool {
        match self {
            PolymorphicEnvelopeBuffer::InMemory(_) => true,
            PolymorphicEnvelopeBuffer::Sqlite(_) => false,
        }
    }

    /// Creates either a memory-based or a disk-based envelope buffer,
    /// depending on the given configuration.
    pub async fn from_config(
        partition_id: u8,
        config: &Config,
        memory_checker: MemoryChecker,
    ) -> Result<Self, EnvelopeBufferError> {
        let buffer = if config.spool_envelopes_path(partition_id).is_some() {
            relay_log::trace!("PolymorphicEnvelopeBuffer: initializing sqlite envelope buffer");
            let buffer = EnvelopeBuffer::<SqliteStackProvider>::new(partition_id, config).await?;
            Self::Sqlite(buffer)
        } else {
            relay_log::trace!("PolymorphicEnvelopeBuffer: initializing memory envelope buffer");
            let buffer = EnvelopeBuffer::<MemoryStackProvider>::new(partition_id, memory_checker);
            Self::InMemory(buffer)
        };

        Ok(buffer)
    }

    /// Initializes the envelope buffer.
    pub async fn initialize(&mut self) {
        match self {
            PolymorphicEnvelopeBuffer::InMemory(buffer) => buffer.initialize().await,
            PolymorphicEnvelopeBuffer::Sqlite(buffer) => buffer.initialize().await,
        }
    }

    /// Adds an envelope to the buffer.
    pub async fn push(&mut self, envelope: Box<Envelope>) -> Result<(), EnvelopeBufferError> {
        relay_statsd::metric!(
            histogram(RelayHistograms::BufferEnvelopeBodySize) =
                envelope.items().map(Item::len).sum::<usize>() as u64,
            partition_id = self.partition_tag()
        );

        relay_statsd::metric!(
            timer(RelayTimers::BufferPush),
            partition_id = self.partition_tag(),
            {
                match self {
                    Self::Sqlite(buffer) => buffer.push(envelope).await,
                    Self::InMemory(buffer) => buffer.push(envelope).await,
                }?;
            }
        );
        Ok(())
    }

    /// Returns a reference to the next-in-line envelope.
    pub async fn peek(&mut self) -> Result<Peek, EnvelopeBufferError> {
        relay_statsd::metric!(
            timer(RelayTimers::BufferPeek),
            partition_id = self.partition_tag(),
            {
                match self {
                    Self::Sqlite(buffer) => buffer.peek().await,
                    Self::InMemory(buffer) => buffer.peek().await,
                }
            }
        )
    }

    /// Pops the next-in-line envelope.
    pub async fn pop(&mut self) -> Result<Option<Box<Envelope>>, EnvelopeBufferError> {
        let envelope = relay_statsd::metric!(
            timer(RelayTimers::BufferPop),
            partition_id = self.partition_tag(),
            {
                match self {
                    Self::Sqlite(buffer) => buffer.pop().await,
                    Self::InMemory(buffer) => buffer.pop().await,
                }?
            }
        );
        Ok(envelope)
    }

    /// Marks a project as ready or not ready.
    ///
    /// The buffer re-prioritizes its envelopes based on this information.
    /// Returns `true` if at least one priority was changed.
    pub fn mark_ready(&mut self, project: &ProjectKey, is_ready: bool) -> bool {
        relay_log::trace!(
            project_key = project.as_str(),
            "buffer marked {}",
            if is_ready { "ready" } else { "not ready" }
        );
        match self {
            Self::Sqlite(buffer) => buffer.mark_ready(project, is_ready),
            Self::InMemory(buffer) => buffer.mark_ready(project, is_ready),
        }
    }

    /// Marks a stack as seen.
    ///
    /// Non-ready stacks are deprioritized when they are marked as seen, such that
    /// the next call to `.peek()` will look at a different stack. This prevents
    /// head-of-line blocking.
    pub fn mark_seen(&mut self, project_key_pair: &ProjectKeyPair, next_fetch: Duration) {
        match self {
            Self::Sqlite(buffer) => buffer.mark_seen(project_key_pair, next_fetch),
            Self::InMemory(buffer) => buffer.mark_seen(project_key_pair, next_fetch),
        }
    }

    /// Returns `true` whether the buffer has capacity to accept new [`Envelope`]s.
    pub fn has_capacity(&self) -> bool {
        match self {
            Self::Sqlite(buffer) => buffer.has_capacity(),
            Self::InMemory(buffer) => buffer.has_capacity(),
        }
    }

    /// Returns the total number of envelopes that have been spooled since the startup. It does
    /// not include the count that existed in a persistent spooler before.
    pub fn item_count(&self) -> u64 {
        match self {
            Self::Sqlite(buffer) => buffer.tracked_count,
            Self::InMemory(buffer) => buffer.tracked_count,
        }
    }

    /// Returns the total number of bytes that the spooler storage uses or `None` if the number
    /// cannot be reliably determined.
    pub fn total_size(&self) -> Option<u64> {
        match self {
            Self::Sqlite(buffer) => buffer.stack_provider.total_size(),
            Self::InMemory(buffer) => buffer.stack_provider.total_size(),
        }
    }

    /// Shuts down the [`PolymorphicEnvelopeBuffer`].
    pub async fn shutdown(&mut self) -> bool {
        // Currently, we want to flush the buffer only for disk, since the in memory implementation
        // tries to not do anything and pop as many elements as possible within the shutdown
        // timeout.
        let Self::Sqlite(buffer) = self else {
            relay_log::trace!("PolymorphicEnvelopeBuffer: shutdown procedure not needed");
            return false;
        };
        buffer.flush().await;

        true
    }

    /// Returns the partition tag for this [`PolymorphicEnvelopeBuffer`].
    fn partition_tag(&self) -> &str {
        match self {
            PolymorphicEnvelopeBuffer::InMemory(buffer) => &buffer.partition_tag,
            PolymorphicEnvelopeBuffer::Sqlite(buffer) => &buffer.partition_tag,
        }
    }
}

/// Error that occurs while interacting with the envelope buffer.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeBufferError {
    #[error("sqlite")]
    SqliteStore(#[from] SqliteEnvelopeStoreError),

    #[error("sqlite")]
    SqliteStack(#[from] SqliteEnvelopeStackError),

    #[error("failed to push envelope to the buffer")]
    PushFailed,
}

impl From<Infallible> for EnvelopeBufferError {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}

/// An envelope buffer that holds an individual stack for each project/sampling project combination.
///
/// Envelope stacks are organized in a priority queue, and are re-prioritized every time an envelope
/// is pushed, popped, or when a project becomes ready.
#[derive(Debug)]
struct EnvelopeBuffer<P: StackProvider> {
    /// The central priority queue.
    priority_queue: priority_queue::PriorityQueue<QueueItem<ProjectKeyPair, P::Stack>, Priority>,
    /// A lookup table to find all stacks involving a project.
    stacks_by_project: hashbrown::HashMap<ProjectKey, BTreeSet<ProjectKeyPair>>,
    /// A provider of stacks that provides utilities to create stacks, check their capacity...
    ///
    /// This indirection is needed because different stack implementations might need different
    /// initialization (e.g. a database connection).
    stack_provider: P,
    /// The total count of envelopes that the buffer is working with.
    ///
    /// Note that this count is not meant to be perfectly accurate since the initialization of the
    /// count might not succeed if it takes more than a set timeout. For example, if we load the
    /// count of all envelopes from disk, and it takes more than the time we set, we will mark the
    /// initial count as 0 and just count incoming and outgoing envelopes from the buffer.
    total_count: i64,
    /// The total count of envelopes that the buffer is working with ignoring envelopes that
    /// were previously stored on disk.
    ///
    /// On startup this will always be 0 and will only count incoming envelopes. If a reliable
    /// count of currently buffered envelopes is required, prefer this over `total_count`
    tracked_count: u64,
    /// Whether the count initialization succeeded or not.
    ///
    /// This boolean is just used for tagging the metric that tracks the total count of envelopes
    /// in the buffer.
    total_count_initialized: bool,
    /// The tag value of this partition which is used for reporting purposes.
    partition_tag: String,
}

impl EnvelopeBuffer<MemoryStackProvider> {
    /// Creates an empty memory-based buffer.
    pub fn new(partition_id: u8, memory_checker: MemoryChecker) -> Self {
        Self {
            stacks_by_project: Default::default(),
            priority_queue: Default::default(),
            stack_provider: MemoryStackProvider::new(memory_checker),
            total_count: 0,
            tracked_count: 0,
            total_count_initialized: false,
            partition_tag: partition_id.to_string(),
        }
    }
}

#[allow(dead_code)]
impl EnvelopeBuffer<SqliteStackProvider> {
    /// Creates an empty sqlite-based buffer.
    pub async fn new(partition_id: u8, config: &Config) -> Result<Self, EnvelopeBufferError> {
        Ok(Self {
            stacks_by_project: Default::default(),
            priority_queue: Default::default(),
            stack_provider: SqliteStackProvider::new(partition_id, config).await?,
            total_count: 0,
            tracked_count: 0,
            total_count_initialized: false,
            partition_tag: partition_id.to_string(),
        })
    }
}

impl<P: StackProvider> EnvelopeBuffer<P>
where
    EnvelopeBufferError: From<<P::Stack as EnvelopeStack>::Error>,
{
    /// Initializes the [`EnvelopeBuffer`] given the initialization state from the
    /// [`StackProvider`].
    pub async fn initialize(&mut self) {
        relay_statsd::metric!(
            timer(RelayTimers::BufferInitialization),
            partition_id = &self.partition_tag,
            {
                let initialization_state = self.stack_provider.initialize().await;
                self.load_stacks(initialization_state.project_key_pairs)
                    .await;
                self.load_store_total_count().await;
            }
        );
    }

    /// Pushes an envelope to the appropriate envelope stack and re-prioritizes the stack.
    ///
    /// If the envelope stack does not exist, a new stack is pushed to the priority queue.
    /// The priority of the stack is updated with the envelope's received_at time.
    pub async fn push(&mut self, envelope: Box<Envelope>) -> Result<(), EnvelopeBufferError> {
        let received_at = envelope.received_at();

        let project_key_pair = ProjectKeyPair::from_envelope(&envelope);
        if let Some((
            QueueItem {
                key: _,
                value: stack,
            },
            _,
        )) = self.priority_queue.get_mut(&project_key_pair)
        {
            stack.push(envelope).await?;
        } else {
            // Since we have initialization code that creates all the necessary stacks, we assume
            // that any new stack that is added during the envelope buffer's lifecycle, is recreated.
            self.push_stack(
                StackCreationType::New,
                ProjectKeyPair::from_envelope(&envelope),
                Some(envelope),
            )
            .await?;
        }
        self.priority_queue
            .change_priority_by(&project_key_pair, |prio| {
                prio.received_at = received_at;
            });

        self.total_count += 1;
        self.tracked_count += 1;
        self.track_total_count();

        Ok(())
    }

    /// Returns a reference to the next-in-line envelope, if one exists.
    pub async fn peek(&mut self) -> Result<Peek, EnvelopeBufferError> {
        let Some((
            QueueItem {
                key: project_key_pair,
                value: stack,
            },
            Priority {
                readiness,
                next_project_fetch,
                ..
            },
        )) = self.priority_queue.peek_mut()
        else {
            return Ok(Peek::Empty);
        };

        let ready = readiness.ready();

        Ok(match (stack.peek().await?, ready) {
            (None, _) => Peek::Empty,
            (Some(last_received_at), true) => Peek::Ready {
                project_key_pair: *project_key_pair,
                last_received_at,
            },
            (Some(last_received_at), false) => Peek::NotReady {
                project_key_pair: *project_key_pair,
                next_project_fetch: *next_project_fetch,
                last_received_at,
            },
        })
    }

    /// Returns the next-in-line envelope, if one exists.
    ///
    /// The priority of the envelope's stack is updated with the next envelope's received_at
    /// time. If the stack is empty after popping, it is removed from the priority queue.
    pub async fn pop(&mut self) -> Result<Option<Box<Envelope>>, EnvelopeBufferError> {
        let Some((QueueItem { key, value: stack }, _)) = self.priority_queue.peek_mut() else {
            return Ok(None);
        };
        let project_key_pair = *key;
        let envelope = stack.pop().await?.expect("found an empty stack");

        let last_received_at = stack.peek().await?;

        match last_received_at {
            None => {
                self.pop_stack(project_key_pair);
            }
            Some(last_received_at) => {
                self.priority_queue
                    .change_priority_by(&project_key_pair, |prio| {
                        prio.received_at = last_received_at;
                    });
            }
        }

        // We are fine with the count going negative, since it represents that more data was popped,
        // than it was initially counted, meaning that we had a wrong total count from
        // initialization.
        self.total_count -= 1;
        self.tracked_count = self.tracked_count.saturating_sub(1);
        self.track_total_count();

        Ok(Some(envelope))
    }

    /// Re-prioritizes all stacks that involve the given project key by setting it to "ready".
    ///
    /// Returns `true` if at least one priority was changed.
    pub fn mark_ready(&mut self, project: &ProjectKey, is_ready: bool) -> bool {
        let mut changed = false;
        if let Some(project_key_pairs) = self.stacks_by_project.get(project) {
            for project_key_pair in project_key_pairs {
                self.priority_queue
                    .change_priority_by(project_key_pair, |stack| {
                        let mut found = false;
                        for (subkey, readiness) in [
                            (
                                project_key_pair.own_key,
                                &mut stack.readiness.own_project_ready,
                            ),
                            (
                                project_key_pair.sampling_key,
                                &mut stack.readiness.sampling_project_ready,
                            ),
                        ] {
                            if subkey == *project {
                                found = true;
                                if *readiness != is_ready {
                                    changed = true;
                                    *readiness = is_ready;
                                }
                            }
                        }
                        debug_assert!(found);
                    });
            }
        }

        changed
    }

    /// Marks a stack as seen.
    ///
    /// Non-ready stacks are deprioritized when they are marked as seen, such that
    /// the next call to `.peek()` will look at a different stack. This prevents
    /// head-of-line blocking.
    pub fn mark_seen(&mut self, project_key_pair: &ProjectKeyPair, next_fetch: Duration) {
        self.priority_queue
            .change_priority_by(project_key_pair, |stack| {
                // We use the next project fetch to debounce project fetching and avoid head of
                // line blocking of non-ready stacks.
                stack.next_project_fetch = Instant::now() + next_fetch;
            });
    }

    /// Returns `true` if the underlying storage has the capacity to store more envelopes.
    pub fn has_capacity(&self) -> bool {
        self.stack_provider.has_store_capacity()
    }

    /// Flushes the envelope buffer.
    pub async fn flush(&mut self) {
        let priority_queue = mem::take(&mut self.priority_queue);
        self.stack_provider
            .flush(priority_queue.into_iter().map(|(q, _)| q.value))
            .await;
    }

    /// Pushes a new [`EnvelopeStack`] with the given [`Envelope`] inserted.
    async fn push_stack(
        &mut self,
        stack_creation_type: StackCreationType,
        project_key_pair: ProjectKeyPair,
        envelope: Option<Box<Envelope>>,
    ) -> Result<(), EnvelopeBufferError> {
        let received_at = envelope.as_ref().map_or(Utc::now(), |e| e.received_at());

        let mut stack = self
            .stack_provider
            .create_stack(stack_creation_type, project_key_pair);
        if let Some(envelope) = envelope {
            stack.push(envelope).await?;
        }

        let previous_entry = self.priority_queue.push(
            QueueItem {
                key: project_key_pair,
                value: stack,
            },
            Priority::new(received_at),
        );
        debug_assert!(previous_entry.is_none());
        for project_key in project_key_pair.iter() {
            self.stacks_by_project
                .entry(project_key)
                .or_default()
                .insert(project_key_pair);
        }
        relay_statsd::metric!(
            gauge(RelayGauges::BufferStackCount) = self.priority_queue.len() as u64,
            partition_id = &self.partition_tag
        );

        Ok(())
    }

    /// Pops an [`EnvelopeStack`] with the supplied [`EnvelopeBufferError`].
    fn pop_stack(&mut self, project_key_pair: ProjectKeyPair) {
        for project_key in project_key_pair.iter() {
            self.stacks_by_project
                .get_mut(&project_key)
                .expect("project_key is missing from lookup")
                .remove(&project_key_pair);
        }
        self.priority_queue.remove(&project_key_pair);

        relay_statsd::metric!(
            gauge(RelayGauges::BufferStackCount) = self.priority_queue.len() as u64,
            partition_id = &self.partition_tag
        );
    }

    /// Creates all the [`EnvelopeStack`]s with no data given a set of [`ProjectKeyPair`].
    async fn load_stacks(&mut self, project_key_pairs: HashSet<ProjectKeyPair>) {
        for project_key_pair in project_key_pairs {
            self.push_stack(StackCreationType::Initialization, project_key_pair, None)
                .await
                .expect("Pushing an empty stack raised an error");
        }
    }

    /// Loads the total count from the store if it takes less than a specified duration.
    ///
    /// The total count returned by the store is related to the count of elements that the buffer
    /// will process, besides the count of elements that will be added and removed during its
    /// lifecycle
    async fn load_store_total_count(&mut self) {
        let total_count = timeout(Duration::from_secs(1), async {
            self.stack_provider.store_total_count().await
        })
        .await;
        match total_count {
            Ok(total_count) => {
                self.total_count = total_count as i64;
                self.total_count_initialized = true;
            }
            Err(error) => {
                self.total_count_initialized = false;
                relay_log::error!(
                    error = &error as &dyn Error,
                    "failed to load the total envelope count of the store",
                );
            }
        };
        self.track_total_count();
    }

    /// Emits a metric to track the total count of envelopes that are in the envelope buffer.
    fn track_total_count(&self) {
        let total_count = self.total_count as f64;
        let initialized = match self.total_count_initialized {
            true => "true",
            false => "false",
        };
        relay_statsd::metric!(
            histogram(RelayHistograms::BufferEnvelopesCount) = total_count,
            initialized = initialized,
            stack_type = self.stack_provider.stack_type(),
            partition_id = &self.partition_tag
        );
    }
}

/// Contains the state of the first element in the buffer.
pub enum Peek {
    Empty,
    Ready {
        project_key_pair: ProjectKeyPair,
        last_received_at: DateTime<Utc>,
    },
    NotReady {
        project_key_pair: ProjectKeyPair,
        next_project_fetch: Instant,
        last_received_at: DateTime<Utc>,
    },
}

impl Peek {
    pub fn last_received_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Empty => None,
            Self::Ready {
                last_received_at, ..
            }
            | Self::NotReady {
                last_received_at, ..
            } => Some(*last_received_at),
        }
    }
}

#[derive(Debug)]
struct QueueItem<K, V> {
    key: K,
    value: V,
}

impl<K, V> std::borrow::Borrow<K> for QueueItem<K, V> {
    fn borrow(&self) -> &K {
        &self.key
    }
}

impl<K: std::hash::Hash, V> std::hash::Hash for QueueItem<K, V> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl<K: PartialEq, V> PartialEq for QueueItem<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: PartialEq, V> Eq for QueueItem<K, V> {}

#[derive(Debug, Clone)]
struct Priority {
    readiness: Readiness,
    received_at: DateTime<Utc>,
    next_project_fetch: Instant,
}

impl Priority {
    fn new(received_at: DateTime<Utc>) -> Self {
        Self {
            readiness: Readiness::new(),
            received_at,
            next_project_fetch: Instant::now(),
        }
    }
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.readiness.ready(), other.readiness.ready()) {
            // Assuming that two priorities differ only w.r.t. the `last_peek`, we want to prioritize
            // stacks that were the least recently peeked. The rationale behind this is that we want
            // to keep cycling through different stacks while peeking.
            (true, true) => self.received_at.cmp(&other.received_at),
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // For non-ready stacks, we invert the priority, such that projects that are not
            // ready and did not receive envelopes recently can be evicted.
            (false, false) => self
                .next_project_fetch
                .cmp(&other.next_project_fetch)
                .reverse()
                .then(self.received_at.cmp(&other.received_at).reverse()),
        }
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Priority {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Priority {}

#[derive(Debug, Clone, Copy)]
struct Readiness {
    own_project_ready: bool,
    sampling_project_ready: bool,
}

impl Readiness {
    fn new() -> Self {
        // Optimistically set ready state to true.
        // The large majority of stack creations are re-creations after a stack was emptied.
        Self {
            own_project_ready: true,
            sampling_project_ready: true,
        }
    }

    fn ready(&self) -> bool {
        self.own_project_ready && self.sampling_project_ready
    }
}

#[cfg(test)]
mod tests {
    use relay_common::Dsn;
    use relay_event_schema::protocol::EventId;
    use relay_sampling::DynamicSamplingContext;
    use std::str::FromStr;
    use std::sync::Arc;
    use uuid::Uuid;

    use crate::envelope::{Item, ItemType};
    use crate::extractors::RequestMeta;
    use crate::services::buffer::common::ProjectKeyPair;
    use crate::services::buffer::envelope_store::sqlite::DatabaseEnvelope;
    use crate::services::buffer::testutils::utils::mock_envelopes;
    use crate::utils::MemoryStat;
    use crate::SqliteEnvelopeStore;

    use super::*;

    impl Peek {
        fn is_empty(&self) -> bool {
            matches!(self, Peek::Empty)
        }
    }

    fn new_envelope(
        own_key: ProjectKey,
        sampling_key: Option<ProjectKey>,
        event_id: Option<EventId>,
    ) -> Box<Envelope> {
        let mut envelope = Envelope::from_request(
            None,
            RequestMeta::new(Dsn::from_str(&format!("http://{own_key}@localhost/1")).unwrap()),
        );
        if let Some(sampling_key) = sampling_key {
            envelope.set_dsc(DynamicSamplingContext {
                public_key: sampling_key,
                trace_id: "67e5504410b1426f9247bb680e5fe0c8".parse().unwrap(),
                release: None,
                user: Default::default(),
                replay_id: None,
                environment: None,
                transaction: None,
                sample_rate: None,
                sampled: None,
                other: Default::default(),
            });
            envelope.add_item(Item::new(ItemType::Transaction));
        }
        if let Some(event_id) = event_id {
            envelope.set_event_id(event_id);
        }
        envelope
    }

    fn mock_config(path: &str) -> Arc<Config> {
        Config::from_json_value(serde_json::json!({
            "spool": {
                "envelopes": {
                    "path": path
                }
            }
        }))
        .unwrap()
        .into()
    }

    fn mock_memory_checker() -> MemoryChecker {
        MemoryChecker::new(MemoryStat::default(), mock_config("my/db/path").clone())
    }

    async fn peek_received_at(buffer: &mut EnvelopeBuffer<MemoryStackProvider>) -> DateTime<Utc> {
        buffer.peek().await.unwrap().last_received_at().unwrap()
    }

    #[tokio::test]
    async fn test_insert_pop() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new(0, mock_memory_checker());

        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let project_key3 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fef").unwrap();

        assert!(buffer.pop().await.unwrap().is_none());
        assert!(buffer.peek().await.unwrap().is_empty());

        let envelope1 = new_envelope(project_key1, None, None);
        let time1 = envelope1.meta().received_at();
        buffer.push(envelope1).await.unwrap();

        let envelope2 = new_envelope(project_key2, None, None);
        let time2 = envelope2.meta().received_at();
        buffer.push(envelope2).await.unwrap();

        // Both projects are ready, so project 2 is on top (has the newest envelopes):
        assert_eq!(peek_received_at(&mut buffer).await, time2);

        buffer.mark_ready(&project_key1, false);
        buffer.mark_ready(&project_key2, false);

        // Both projects are not ready, so project 1 is on top (has the oldest envelopes):
        assert_eq!(peek_received_at(&mut buffer).await, time1);

        let envelope3 = new_envelope(project_key3, None, None);
        let time3 = envelope3.meta().received_at();
        buffer.push(envelope3).await.unwrap();
        buffer.mark_ready(&project_key3, false);

        // All projects are not ready, so project 1 is on top (has the oldest envelopes):
        assert_eq!(peek_received_at(&mut buffer).await, time1);

        // After marking a project ready, it goes to the top:
        buffer.mark_ready(&project_key3, true);
        assert_eq!(peek_received_at(&mut buffer).await, time3);
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().public_key(),
            project_key3
        );

        // After popping, project 1 is on top again:
        assert_eq!(peek_received_at(&mut buffer).await, time1);

        // Mark project 1 as ready (still on top):
        buffer.mark_ready(&project_key1, true);
        assert_eq!(peek_received_at(&mut buffer).await, time1);

        // Mark project 2 as ready as well (now on top because most recent):
        buffer.mark_ready(&project_key2, true);
        assert_eq!(peek_received_at(&mut buffer).await, time2);
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().public_key(),
            project_key2
        );

        // Pop last element:
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );
        assert!(buffer.pop().await.unwrap().is_none());
        assert!(buffer.peek().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_project_internal_order() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new(0, mock_memory_checker());

        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();

        let envelope1 = new_envelope(project_key, None, None);
        let time1 = envelope1.meta().received_at();
        let envelope2 = new_envelope(project_key, None, None);
        let time2 = envelope2.meta().received_at();

        assert!(time2 > time1);

        buffer.push(envelope1).await.unwrap();
        buffer.push(envelope2).await.unwrap();

        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().received_at(),
            time2
        );
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().received_at(),
            time1
        );
        assert!(buffer.pop().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_sampling_projects() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new(0, mock_memory_checker());

        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fef").unwrap();

        let envelope1 = new_envelope(project_key1, None, None);
        let time1 = envelope1.received_at();
        buffer.push(envelope1).await.unwrap();

        let envelope2 = new_envelope(project_key2, None, None);
        let time2 = envelope2.received_at();
        buffer.push(envelope2).await.unwrap();

        let envelope3 = new_envelope(project_key1, Some(project_key2), None);
        let time3 = envelope3.meta().received_at();
        buffer.push(envelope3).await.unwrap();

        buffer.mark_ready(&project_key1, false);
        buffer.mark_ready(&project_key2, false);

        // Nothing is ready, instant1 is on top:
        assert_eq!(
            buffer.peek().await.unwrap().last_received_at().unwrap(),
            time1
        );

        // Mark project 2 ready, gets on top:
        buffer.mark_ready(&project_key2, true);
        assert_eq!(
            buffer.peek().await.unwrap().last_received_at().unwrap(),
            time2
        );

        // Revert
        buffer.mark_ready(&project_key2, false);
        assert_eq!(
            buffer.peek().await.unwrap().last_received_at().unwrap(),
            time1
        );

        // Project 1 ready:
        buffer.mark_ready(&project_key1, true);
        assert_eq!(
            buffer.peek().await.unwrap().last_received_at().unwrap(),
            time1
        );

        // when both projects are ready, event no 3 ends up on top:
        buffer.mark_ready(&project_key2, true);
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().received_at(),
            time3
        );
        assert_eq!(
            buffer.peek().await.unwrap().last_received_at().unwrap(),
            time2
        );

        buffer.mark_ready(&project_key2, false);
        assert_eq!(buffer.pop().await.unwrap().unwrap().received_at(), time1);
        assert_eq!(buffer.pop().await.unwrap().unwrap().received_at(), time2);

        assert!(buffer.pop().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_project_keys_distinct() {
        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fef").unwrap();

        let project_key_pair1 = ProjectKeyPair::new(project_key1, project_key2);
        let project_key_pair2 = ProjectKeyPair::new(project_key2, project_key1);

        assert_ne!(project_key_pair1, project_key_pair2);

        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new(0, mock_memory_checker());
        buffer
            .push(new_envelope(project_key1, Some(project_key2), None))
            .await
            .unwrap();
        buffer
            .push(new_envelope(project_key2, Some(project_key1), None))
            .await
            .unwrap();
        assert_eq!(buffer.priority_queue.len(), 2);
    }

    #[test]
    fn test_total_order() {
        let p1 = Priority {
            readiness: Readiness {
                own_project_ready: true,
                sampling_project_ready: true,
            },
            received_at: Utc::now(),
            next_project_fetch: Instant::now(),
        };
        let mut p2 = p1.clone();
        p2.next_project_fetch += Duration::from_millis(1);

        // Last peek does not matter because project is ready:
        assert_eq!(p1.cmp(&p2), Ordering::Equal);
        assert_eq!(p1, p2);
    }

    #[tokio::test]
    async fn test_last_peek_internal_order() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new(0, mock_memory_checker());

        let project_key_1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let event_id_1 = EventId::new();
        let envelope1 = new_envelope(project_key_1, None, Some(event_id_1));
        let time1 = envelope1.received_at();

        let project_key_2 = ProjectKey::parse("b56ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let event_id_2 = EventId::new();
        let envelope2 = new_envelope(project_key_2, None, Some(event_id_2));
        let time2 = envelope2.received_at();

        buffer.push(envelope1).await.unwrap();
        buffer.push(envelope2).await.unwrap();

        buffer.mark_ready(&project_key_1, false);
        buffer.mark_ready(&project_key_2, false);

        // event_id_1 is first element:
        let Peek::NotReady {
            last_received_at, ..
        } = buffer.peek().await.unwrap()
        else {
            panic!();
        };
        assert_eq!(last_received_at, time1);

        // Second peek returns same element:
        let Peek::NotReady {
            last_received_at,
            project_key_pair,
            ..
        } = buffer.peek().await.unwrap()
        else {
            panic!();
        };
        assert_eq!(last_received_at, time1);
        assert_ne!(last_received_at, time2);

        buffer.mark_seen(&project_key_pair, Duration::ZERO);

        // After mark_seen, event 2 is on top:
        let Peek::NotReady {
            last_received_at, ..
        } = buffer.peek().await.unwrap()
        else {
            panic!();
        };
        assert_eq!(last_received_at, time2);
        assert_ne!(last_received_at, time1);

        let Peek::NotReady {
            last_received_at,
            project_key_pair,
            ..
        } = buffer.peek().await.unwrap()
        else {
            panic!();
        };
        assert_eq!(last_received_at, time2);
        assert_ne!(last_received_at, time1);

        buffer.mark_seen(&project_key_pair, Duration::ZERO);

        // After another mark_seen, cycle back to event 1:
        let Peek::NotReady {
            last_received_at, ..
        } = buffer.peek().await.unwrap()
        else {
            panic!();
        };
        assert_eq!(last_received_at, time1);
        assert_ne!(last_received_at, time2);
    }

    #[tokio::test]
    async fn test_initialize_buffer() {
        let path = std::env::temp_dir()
            .join(Uuid::new_v4().to_string())
            .into_os_string()
            .into_string()
            .unwrap();
        let config = mock_config(&path);
        let mut store = SqliteEnvelopeStore::prepare(0, &config).await.unwrap();
        let mut buffer = EnvelopeBuffer::<SqliteStackProvider>::new(0, &config)
            .await
            .unwrap();

        // We write 5 envelopes to disk so that we can check if they are loaded. These envelopes
        // belong to the same project keys, so they belong to the same envelope stack.
        let envelopes = mock_envelopes(10);
        assert!(store
            .insert_batch(
                envelopes
                    .into_iter()
                    .map(|e| DatabaseEnvelope::try_from(e.as_ref()).unwrap())
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap()
            )
            .await
            .is_ok());

        // We assume that the buffer is empty.
        assert!(buffer.priority_queue.is_empty());
        assert!(buffer.stacks_by_project.is_empty());

        buffer.initialize().await;

        // We assume that we loaded only 1 envelope stack, because of the project keys combinations
        // of the envelopes we inserted above.
        assert_eq!(buffer.priority_queue.len(), 1);
        // We expect to have an entry per project key, since we have 1 pair, the total entries
        // should be 2.
        assert_eq!(buffer.stacks_by_project.len(), 2);
    }
}
