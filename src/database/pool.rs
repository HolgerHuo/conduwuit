mod configure;

use std::{
	mem::take,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

use async_channel::{QueueStrategy, Receiver, RecvError, Sender};
use conduwuit::{
	debug, debug_warn, defer, err, implement,
	result::DebugInspect,
	trace,
	utils::sys::compute::{get_affinity, nth_core_available, set_affinity},
	Result, Server,
};
use futures::{channel::oneshot, TryFutureExt};
use oneshot::Sender as ResultSender;
use rocksdb::Direction;
use smallvec::SmallVec;
use tokio::task::JoinSet;

use self::configure::configure;
use crate::{keyval::KeyBuf, stream, Handle, Map};

/// Frontend thread-pool. Operating system threads are used to make database
/// requests which are not cached. These thread-blocking requests are offloaded
/// from the tokio async workers and executed on this threadpool.
pub(crate) struct Pool {
	server: Arc<Server>,
	queues: Vec<Sender<Cmd>>,
	workers: Mutex<JoinSet<()>>,
	topology: Vec<usize>,
	busy: AtomicUsize,
	queued_max: AtomicUsize,
}

/// Operations which can be submitted to the pool.
pub(crate) enum Cmd {
	Get(Get),
	Iter(Seek),
}

/// Multi-point-query
pub(crate) struct Get {
	pub(crate) map: Arc<Map>,
	pub(crate) key: BatchQuery<'static>,
	pub(crate) res: Option<ResultSender<BatchResult<'static>>>,
}

/// Iterator-seek.
/// Note: only initial seek is supported at this time on the assumption rocksdb
/// prefetching prevents mid-iteration polls from blocking on I/O.
pub(crate) struct Seek {
	pub(crate) map: Arc<Map>,
	pub(crate) state: stream::State<'static>,
	pub(crate) dir: Direction,
	pub(crate) key: Option<KeyBuf>,
	pub(crate) res: Option<ResultSender<stream::State<'static>>>,
}

pub(crate) type BatchQuery<'a> = SmallVec<[KeyBuf; BATCH_INLINE]>;
pub(crate) type BatchResult<'a> = SmallVec<[ResultHandle<'a>; BATCH_INLINE]>;
pub(crate) type ResultHandle<'a> = Result<Handle<'a>>;

const WORKER_LIMIT: (usize, usize) = (1, 1024);
const QUEUE_LIMIT: (usize, usize) = (1, 2048);
const BATCH_INLINE: usize = 1;

#[implement(Pool)]
pub(crate) async fn new(server: &Arc<Server>) -> Result<Arc<Self>> {
	const CHAN_SCHED: (QueueStrategy, QueueStrategy) = (QueueStrategy::Fifo, QueueStrategy::Lifo);

	let (total_workers, queue_sizes, topology) = configure(server);

	let (senders, receivers) = queue_sizes
		.into_iter()
		.map(|cap| async_channel::bounded_with_queue_strategy(cap, CHAN_SCHED))
		.unzip();

	let pool = Arc::new(Self {
		server: server.clone(),

		queues: senders,

		workers: JoinSet::new().into(),

		topology,

		busy: AtomicUsize::default(),

		queued_max: AtomicUsize::default(),
	});

	pool.spawn_until(receivers, total_workers).await?;

	Ok(pool)
}

impl Drop for Pool {
	fn drop(&mut self) {
		debug_assert!(self.queues.iter().all(Sender::is_empty), "channel must be empty on drop");
		debug_assert!(
			self.queues.iter().all(Sender::is_closed),
			"channel should be closed on drop"
		);
	}
}

#[implement(Pool)]
pub(crate) async fn shutdown(self: &Arc<Self>) {
	self.close();

	let workers = take(&mut *self.workers.lock().expect("locked"));
	debug!(workers = workers.len(), "Waiting for workers to join...");

	workers.join_all().await;
}

#[implement(Pool)]
pub(crate) fn close(&self) {
	let senders = self.queues.iter().map(Sender::sender_count).sum::<usize>();

	let receivers = self
		.queues
		.iter()
		.map(Sender::receiver_count)
		.sum::<usize>();

	debug!(
		queues = self.queues.len(),
		workers = self.workers.lock().expect("locked").len(),
		?senders,
		?receivers,
		"Closing pool..."
	);

	for queue in &self.queues {
		queue.close();
	}

	self.workers.lock().expect("locked").abort_all();
	std::thread::yield_now();
}

#[implement(Pool)]
async fn spawn_until(self: &Arc<Self>, recv: Vec<Receiver<Cmd>>, count: usize) -> Result {
	let mut workers = self.workers.lock().expect("locked");
	while workers.len() < count {
		self.spawn_one(&mut workers, &recv)?;
	}

	Ok(())
}

#[implement(Pool)]
#[tracing::instrument(
	name = "spawn",
	level = "trace",
	skip_all,
	fields(id = %workers.len())
)]
fn spawn_one(self: &Arc<Self>, workers: &mut JoinSet<()>, recv: &[Receiver<Cmd>]) -> Result {
	debug_assert!(!self.queues.is_empty(), "Must have at least one queue");
	debug_assert!(!recv.is_empty(), "Must have at least one receiver");

	let id = workers.len();
	let group = id.overflowing_rem(self.queues.len()).0;
	let recv = recv[group].clone();
	let self_ = self.clone();

	#[cfg(not(tokio_unstable))]
	let _abort = workers.spawn_blocking_on(move || self_.worker(id, recv), self.server.runtime());

	#[cfg(tokio_unstable)]
	let _abort = workers
		.build_task()
		.name("conduwuit:dbpool")
		.spawn_blocking_on(move || self_.worker(id, recv), self.server.runtime());

	Ok(())
}

#[implement(Pool)]
#[tracing::instrument(level = "trace", name = "get", skip(self, cmd))]
pub(crate) async fn execute_get(self: &Arc<Self>, mut cmd: Get) -> Result<BatchResult<'_>> {
	let (send, recv) = oneshot::channel();
	_ = cmd.res.insert(send);

	let queue = self.select_queue();
	self.execute(queue, Cmd::Get(cmd))
		.and_then(move |()| {
			recv.map_ok(into_recv_get)
				.map_err(|e| err!(error!("recv failed {e:?}")))
		})
		.await
		.map(Into::into)
		.map_err(Into::into)
}

#[implement(Pool)]
#[tracing::instrument(level = "trace", name = "iter", skip(self, cmd))]
pub(crate) async fn execute_iter(self: &Arc<Self>, mut cmd: Seek) -> Result<stream::State<'_>> {
	let (send, recv) = oneshot::channel();
	_ = cmd.res.insert(send);

	let queue = self.select_queue();
	self.execute(queue, Cmd::Iter(cmd))
		.and_then(|()| {
			recv.map_ok(into_recv_seek)
				.map_err(|e| err!(error!("recv failed {e:?}")))
		})
		.await
}

#[implement(Pool)]
fn select_queue(&self) -> &Sender<Cmd> {
	let core_id = get_affinity().next().unwrap_or(0);
	let chan_id = self.topology[core_id];
	self.queues.get(chan_id).unwrap_or_else(|| &self.queues[0])
}

#[implement(Pool)]
#[tracing::instrument(
	level = "trace",
	name = "execute",
	skip(self, cmd),
	fields(
		task = ?tokio::task::try_id(),
		receivers = queue.receiver_count(),
		queued = queue.len(),
		queued_max = self.queued_max.load(Ordering::Relaxed),
	),
)]
async fn execute(&self, queue: &Sender<Cmd>, cmd: Cmd) -> Result {
	if cfg!(debug_assertions) {
		self.queued_max.fetch_max(queue.len(), Ordering::Relaxed);
	}

	if queue.is_full() {
		debug_warn!(
			capacity = ?queue.capacity(),
			"pool queue is full"
		);
	}

	queue
		.send(cmd)
		.await
		.map_err(|e| err!(error!("send failed {e:?}")))
}

#[implement(Pool)]
#[tracing::instrument(
	parent = None,
	level = "debug",
	skip(self, recv),
	fields(
		tid = ?std::thread::current().id(),
	),
)]
fn worker(self: Arc<Self>, id: usize, recv: Receiver<Cmd>) {
	defer! {{ trace!("worker finished"); }}
	trace!("worker spawned");

	self.worker_init(id);
	self.worker_loop(&recv);
}

#[implement(Pool)]
fn worker_init(&self, id: usize) {
	let group = id.overflowing_rem(self.queues.len()).0;
	let affinity = self
		.topology
		.iter()
		.enumerate()
		.filter(|_| self.queues.len() > 1)
		.filter_map(|(core_id, &queue_id)| (group == queue_id).then_some(core_id))
		.filter_map(nth_core_available);

	// affinity is empty (no-op) if there's only one queue
	set_affinity(affinity.clone());
	debug!(
		?group,
		affinity = ?affinity.collect::<Vec<_>>(),
		"worker ready"
	);
}

#[implement(Pool)]
fn worker_loop(self: &Arc<Self>, recv: &Receiver<Cmd>) {
	// initial +1 needed prior to entering wait
	self.busy.fetch_add(1, Ordering::Relaxed);

	while let Ok(cmd) = self.worker_wait(recv) {
		self.worker_handle(cmd);
	}
}

#[implement(Pool)]
#[tracing::instrument(
	name = "wait",
	level = "trace",
	skip_all,
	fields(
		receivers = recv.receiver_count(),
		queued = recv.len(),
		busy = self.busy.fetch_sub(1, Ordering::Relaxed) - 1,
	),
)]
fn worker_wait(self: &Arc<Self>, recv: &Receiver<Cmd>) -> Result<Cmd, RecvError> {
	recv.recv_blocking().debug_inspect(|_| {
		self.busy.fetch_add(1, Ordering::Relaxed);
	})
}

#[implement(Pool)]
fn worker_handle(self: &Arc<Self>, cmd: Cmd) {
	match cmd {
		| Cmd::Get(cmd) if cmd.key.len() == 1 => self.handle_get(cmd),
		| Cmd::Get(cmd) => self.handle_batch(cmd),
		| Cmd::Iter(cmd) => self.handle_iter(cmd),
	};
}

#[implement(Pool)]
#[tracing::instrument(
	name = "iter",
	level = "trace",
	skip_all,
	fields(%cmd.map),
)]
fn handle_iter(&self, mut cmd: Seek) {
	let chan = cmd.res.take().expect("missing result channel");

	if chan.is_canceled() {
		return;
	}

	let from = cmd.key.as_deref().map(Into::into);

	let result = match cmd.dir {
		| Direction::Forward => cmd.state.init_fwd(from),
		| Direction::Reverse => cmd.state.init_rev(from),
	};

	let chan_result = chan.send(into_send_seek(result));

	let _chan_sent = chan_result.is_ok();
}

#[implement(Pool)]
#[tracing::instrument(
	name = "batch",
	level = "trace",
	skip_all,
	fields(
		%cmd.map,
		keys = %cmd.key.len(),
	),
)]
fn handle_batch(self: &Arc<Self>, mut cmd: Get) {
	debug_assert!(cmd.key.len() > 1, "should have more than one key");
	debug_assert!(!cmd.key.iter().any(SmallVec::is_empty), "querying for empty key");

	let chan = cmd.res.take().expect("missing result channel");

	if chan.is_canceled() {
		return;
	}

	let keys = cmd.key.iter().map(Into::into);

	let result: SmallVec<_> = cmd.map.get_batch_blocking(keys).collect();

	let chan_result = chan.send(into_send_get(result));

	let _chan_sent = chan_result.is_ok();
}

#[implement(Pool)]
#[tracing::instrument(
	name = "get",
	level = "trace",
	skip_all,
	fields(%cmd.map),
)]
fn handle_get(&self, mut cmd: Get) {
	debug_assert!(!cmd.key[0].is_empty(), "querying for empty key");

	// Obtain the result channel.
	let chan = cmd.res.take().expect("missing result channel");

	// It is worth checking if the future was dropped while the command was queued
	// so we can bail without paying for any query.
	if chan.is_canceled() {
		return;
	}

	// Perform the actual database query. We reuse our database::Map interface but
	// limited to the blocking calls, rather than creating another surface directly
	// with rocksdb here.
	let result = cmd.map.get_blocking(&cmd.key[0]);

	// Send the result back to the submitter.
	let chan_result = chan.send(into_send_get([result].into()));

	// If the future was dropped during the query this will fail acceptably.
	let _chan_sent = chan_result.is_ok();
}

fn into_send_get(result: BatchResult<'_>) -> BatchResult<'static> {
	// SAFETY: Necessary to send the Handle (rust_rocksdb::PinnableSlice) through
	// the channel. The lifetime on the handle is a device by rust-rocksdb to
	// associate a database lifetime with its assets. The Handle must be dropped
	// before the database is dropped.
	unsafe { std::mem::transmute(result) }
}

fn into_recv_get<'a>(result: BatchResult<'static>) -> BatchResult<'a> {
	// SAFETY: This is to receive the Handle from the channel.
	unsafe { std::mem::transmute(result) }
}

pub(crate) fn into_send_seek(result: stream::State<'_>) -> stream::State<'static> {
	// SAFETY: Necessary to send the State through the channel; see above.
	unsafe { std::mem::transmute(result) }
}

fn into_recv_seek(result: stream::State<'static>) -> stream::State<'_> {
	// SAFETY: This is to receive the State from the channel; see above.
	unsafe { std::mem::transmute(result) }
}
