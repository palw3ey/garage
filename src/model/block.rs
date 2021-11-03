use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use futures::future::*;
use futures::select;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Mutex, Notify};

use garage_util::data::*;
use garage_util::error::*;
use garage_util::time::*;
use garage_util::tranquilizer::Tranquilizer;

use garage_rpc::system::System;
use garage_rpc::*;

use garage_table::replication::{TableReplication, TableShardedReplication};

use crate::block_ref_table::*;

use crate::garage::Garage;

/// Size under which data will be stored inlined in database instead of as files
pub const INLINE_THRESHOLD: usize = 3072;

pub const BACKGROUND_WORKERS: u64 = 1;
pub const BACKGROUND_TRANQUILITY: u32 = 3;

const BLOCK_RW_TIMEOUT: Duration = Duration::from_secs(42);
const BLOCK_GC_TIMEOUT: Duration = Duration::from_secs(60);
const NEED_BLOCK_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const RESYNC_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

/// RPC messages used to share blocks of data between nodes
#[derive(Debug, Serialize, Deserialize)]
pub enum BlockRpc {
	Ok,
	/// Message to ask for a block of data, by hash
	GetBlock(Hash),
	/// Message to send a block of data, either because requested, of for first delivery of new
	/// block
	PutBlock(PutBlockMessage),
	/// Ask other node if they should have this block, but don't actually have it
	NeedBlockQuery(Hash),
	/// Response : whether the node do require that block
	NeedBlockReply(bool),
}

/// Structure used to send a block
#[derive(Debug, Serialize, Deserialize)]
pub struct PutBlockMessage {
	/// Hash of the block
	pub hash: Hash,

	/// Content of the block
	#[serde(with = "serde_bytes")]
	pub data: Vec<u8>,
}

impl Rpc for BlockRpc {
	type Response = Result<BlockRpc, Error>;
}

/// The block manager, handling block exchange between nodes, and block storage on local node
pub struct BlockManager {
	/// Replication strategy, allowing to find on which node blocks should be located
	pub replication: TableShardedReplication,
	/// Directory in which block are stored
	pub data_dir: PathBuf,

	mutation_lock: Mutex<BlockManagerLocked>,

	rc: sled::Tree,

	resync_queue: sled::Tree,
	resync_notify: Notify,

	system: Arc<System>,
	endpoint: Arc<Endpoint<BlockRpc, Self>>,
	pub(crate) garage: ArcSwapOption<Garage>,
}

// This custom struct contains functions that must only be ran
// when the lock is held. We ensure that it is the case by storing
// it INSIDE a Mutex.
struct BlockManagerLocked();

impl BlockManager {
	pub fn new(
		db: &sled::Db,
		data_dir: PathBuf,
		replication: TableShardedReplication,
		system: Arc<System>,
	) -> Arc<Self> {
		let rc = db
			.open_tree("block_local_rc")
			.expect("Unable to open block_local_rc tree");

		let resync_queue = db
			.open_tree("block_local_resync_queue")
			.expect("Unable to open block_local_resync_queue tree");

		let endpoint = system
			.netapp
			.endpoint("garage_model/block.rs/Rpc".to_string());

		let manager_locked = BlockManagerLocked();

		let block_manager = Arc::new(Self {
			replication,
			data_dir,
			mutation_lock: Mutex::new(manager_locked),
			rc,
			resync_queue,
			resync_notify: Notify::new(),
			system,
			endpoint,
			garage: ArcSwapOption::from(None),
		});
		block_manager.endpoint.set_handler(block_manager.clone());

		block_manager
	}

	// ---- Public interface ----

	/// Ask nodes that might have a block for it
	pub async fn rpc_get_block(&self, hash: &Hash) -> Result<Vec<u8>, Error> {
		let who = self.replication.read_nodes(hash);
		let resps = self
			.system
			.rpc
			.try_call_many(
				&self.endpoint,
				&who[..],
				BlockRpc::GetBlock(*hash),
				RequestStrategy::with_priority(PRIO_NORMAL)
					.with_quorum(1)
					.with_timeout(BLOCK_RW_TIMEOUT)
					.interrupt_after_quorum(true),
			)
			.await?;

		for resp in resps {
			if let BlockRpc::PutBlock(msg) = resp {
				return Ok(msg.data);
			}
		}
		Err(Error::Message(format!(
			"Unable to read block {:?}: no valid blocks returned",
			hash
		)))
	}

	/// Send block to nodes that should have it
	pub async fn rpc_put_block(&self, hash: Hash, data: Vec<u8>) -> Result<(), Error> {
		let who = self.replication.write_nodes(&hash);
		self.system
			.rpc
			.try_call_many(
				&self.endpoint,
				&who[..],
				BlockRpc::PutBlock(PutBlockMessage { hash, data }),
				RequestStrategy::with_priority(PRIO_NORMAL)
					.with_quorum(self.replication.write_quorum())
					.with_timeout(BLOCK_RW_TIMEOUT),
			)
			.await?;
		Ok(())
	}

	/// Launch the repair procedure on the data store
	///
	/// This will list all blocks locally present, as well as those
	/// that are required because of refcount > 0, and will try
	/// to fix any mismatch between the two.
	pub async fn repair_data_store(&self, must_exit: &watch::Receiver<bool>) -> Result<(), Error> {
		// 1. Repair blocks from RC table
		let garage = self.garage.load_full().unwrap();
		let mut last_hash = None;
		for (i, entry) in garage.block_ref_table.data.store.iter().enumerate() {
			let (_k, v_bytes) = entry?;
			let block_ref = rmp_serde::decode::from_read_ref::<_, BlockRef>(v_bytes.as_ref())?;
			if Some(&block_ref.block) == last_hash.as_ref() {
				continue;
			}
			if !block_ref.deleted.get() {
				last_hash = Some(block_ref.block);
				self.put_to_resync(&block_ref.block, Duration::from_secs(0))?;
			}
			if i & 0xFF == 0 && *must_exit.borrow() {
				return Ok(());
			}
		}

		// 2. Repair blocks actually on disk
		// Lists all blocks on disk and adds them to the resync queue.
		// This allows us to find blocks we are storing but don't actually need,
		// so that we can offload them if necessary and then delete them locally.
		self.for_each_file(
			(),
			move |_, hash| async move { self.put_to_resync(&hash, Duration::from_secs(0)) },
			must_exit,
		)
		.await
	}

	/// Verify integrity of each block on disk. Use `speed_limit` to limit the load generated by
	/// this function.
	pub async fn scrub_data_store(
		&self,
		must_exit: &watch::Receiver<bool>,
		tranquility: u32,
	) -> Result<(), Error> {
		let tranquilizer = Tranquilizer::new(30);
		self.for_each_file(
			tranquilizer,
			move |mut tranquilizer, hash| async move {
				let _ = self.read_block(&hash).await;
				tranquilizer.tranquilize(tranquility).await;
				Ok(tranquilizer)
			},
			must_exit,
		)
		.await
	}

	/// Get lenght of resync queue
	pub fn resync_queue_len(&self) -> usize {
		self.resync_queue.len()
	}

	/// Get number of items in the refcount table
	pub fn rc_len(&self) -> usize {
		self.rc.len()
	}

	//// ----- Managing the reference counter ----

	/// Increment the number of time a block is used, putting it to resynchronization if it is
	/// required, but not known
	pub fn block_incref(&self, hash: &Hash) -> Result<(), Error> {
		let old_rc = self.rc.fetch_and_update(&hash, |old| {
			let old_v = old.map(u64_from_be_bytes).unwrap_or(0);
			Some(u64::to_be_bytes(old_v + 1).to_vec())
		})?;
		let old_rc = old_rc.map(u64_from_be_bytes).unwrap_or(0);
		if old_rc == 0 {
			self.put_to_resync(hash, BLOCK_RW_TIMEOUT)?;
		}
		Ok(())
	}

	/// Decrement the number of time a block is used
	pub fn block_decref(&self, hash: &Hash) -> Result<(), Error> {
		let new_rc = self.rc.update_and_fetch(&hash, |old| {
			let old_v = old.map(u64_from_be_bytes).unwrap_or(0);
			if old_v > 1 {
				Some(u64::to_be_bytes(old_v - 1).to_vec())
			} else {
				None
			}
		})?;
		if new_rc.is_none() {
			self.put_to_resync(hash, BLOCK_GC_TIMEOUT)?;
		}
		Ok(())
	}

	/// Read a block's reference count
	pub fn get_block_rc(&self, hash: &Hash) -> Result<u64, Error> {
		Ok(self
			.rc
			.get(hash.as_ref())?
			.map(u64_from_be_bytes)
			.unwrap_or(0))
	}

	// ---- Reading and writing blocks locally ----

	/// Write a block to disk
	async fn write_block(&self, hash: &Hash, data: &[u8]) -> Result<BlockRpc, Error> {
		self.mutation_lock
			.lock()
			.await
			.write_block(hash, data, self)
			.await
	}

	/// Read block from disk, verifying it's integrity
	async fn read_block(&self, hash: &Hash) -> Result<BlockRpc, Error> {
		let path = self.block_path(hash);

		let mut f = match fs::File::open(&path).await {
			Ok(f) => f,
			Err(e) => {
				// Not found but maybe we should have had it ??
				self.put_to_resync(hash, Duration::from_millis(0))?;
				return Err(Into::into(e));
			}
		};
		let mut data = vec![];
		f.read_to_end(&mut data).await?;
		drop(f);

		if blake2sum(&data[..]) != *hash {
			self.mutation_lock
				.lock()
				.await
				.move_block_to_corrupted(hash, self)
				.await?;
			return Err(Error::CorruptData(*hash));
		}

		Ok(BlockRpc::PutBlock(PutBlockMessage { hash: *hash, data }))
	}

	/// Check if this node should have a block, but don't actually have it
	async fn need_block(&self, hash: &Hash) -> Result<bool, Error> {
		let BlockStatus { exists, needed } = self
			.mutation_lock
			.lock()
			.await
			.check_block_status(hash, self)
			.await?;
		Ok(needed && !exists)
	}

	/// Utility: gives the path of the directory in which a block should be found
	fn block_dir(&self, hash: &Hash) -> PathBuf {
		let mut path = self.data_dir.clone();
		path.push(hex::encode(&hash.as_slice()[0..1]));
		path.push(hex::encode(&hash.as_slice()[1..2]));
		path
	}

	/// Utility: give the full path where a block should be found
	fn block_path(&self, hash: &Hash) -> PathBuf {
		let mut path = self.block_dir(hash);
		path.push(hex::encode(hash.as_ref()));
		path
	}

	// ---- Resync loop ----

	pub fn spawn_background_worker(self: Arc<Self>) {
		// Launch 2 simultaneous workers for background resync loop preprocessing
		for i in 0..BACKGROUND_WORKERS {
			let bm2 = self.clone();
			let background = self.system.background.clone();
			tokio::spawn(async move {
				tokio::time::sleep(Duration::from_secs(10 * (i + 1))).await;
				background.spawn_worker(format!("block resync worker {}", i), move |must_exit| {
					bm2.resync_loop(must_exit)
				});
			});
		}
	}

	fn put_to_resync(&self, hash: &Hash, delay: Duration) -> Result<(), Error> {
		let when = now_msec() + delay.as_millis() as u64;
		trace!("Put resync_queue: {} {:?}", when, hash);
		let mut key = u64::to_be_bytes(when).to_vec();
		key.extend(hash.as_ref());
		self.resync_queue.insert(key, hash.as_ref())?;
		self.resync_notify.notify_waiters();
		Ok(())
	}

	async fn resync_loop(self: Arc<Self>, mut must_exit: watch::Receiver<bool>) {
		let mut tranquilizer = Tranquilizer::new(30);

		while !*must_exit.borrow() {
			match self.resync_iter(&mut must_exit).await {
				Ok(true) => {
					tranquilizer.tranquilize(BACKGROUND_TRANQUILITY).await;
				}
				Ok(false) => {
					tranquilizer.reset();
				}
				Err(e) => {
					// The errors that we have here are only Sled errors
					// We don't really know how to handle them so just ¯\_(ツ)_/¯
					// (there is kind of an assumption that Sled won't error on us,
					// if it does there is not much we can do -- TODO should we just panic?)
					error!(
						"Could not do a resync iteration: {} (this is a very bad error)",
						e
					);
					tranquilizer.reset();
				}
			}
		}
	}

	async fn resync_iter(&self, must_exit: &mut watch::Receiver<bool>) -> Result<bool, Error> {
		if let Some((time_bytes, hash_bytes)) = self.resync_queue.pop_min()? {
			let time_msec = u64_from_be_bytes(&time_bytes[0..8]);
			let now = now_msec();
			if now >= time_msec {
				let hash = Hash::try_from(&hash_bytes[..]).unwrap();
				let res = self.resync_block(&hash).await;
				if let Err(e) = &res {
					warn!("Error when resyncing {:?}: {}", hash, e);
					self.put_to_resync(&hash, RESYNC_RETRY_TIMEOUT)?;
				}
				Ok(true)
			} else {
				self.resync_queue.insert(time_bytes, hash_bytes)?;
				let delay = tokio::time::sleep(Duration::from_millis(time_msec - now));
				select! {
					_ = delay.fuse() => {},
					_ = self.resync_notify.notified().fuse() => {},
					_ = must_exit.changed().fuse() => {},
				}
				Ok(false)
			}
		} else {
			select! {
				_ = self.resync_notify.notified().fuse() => {},
				_ = must_exit.changed().fuse() => {},
			}
			Ok(false)
		}
	}

	async fn resync_block(&self, hash: &Hash) -> Result<(), Error> {
		let BlockStatus { exists, needed } = self
			.mutation_lock
			.lock()
			.await
			.check_block_status(hash, self)
			.await?;

		if exists != needed {
			info!(
				"Resync block {:?}: exists {}, needed {}",
				hash, exists, needed
			);
		}

		if exists && !needed {
			trace!("Offloading block {:?}", hash);

			let mut who = self.replication.write_nodes(hash);
			if who.len() < self.replication.write_quorum() {
				return Err(Error::Message("Not trying to offload block because we don't have a quorum of nodes to write to".to_string()));
			}
			who.retain(|id| *id != self.system.id);

			let msg = Arc::new(BlockRpc::NeedBlockQuery(*hash));
			let who_needs_fut = who.iter().map(|to| {
				self.system.rpc.call_arc(
					&self.endpoint,
					*to,
					msg.clone(),
					RequestStrategy::with_priority(PRIO_BACKGROUND)
						.with_timeout(NEED_BLOCK_QUERY_TIMEOUT),
				)
			});
			let who_needs_resps = join_all(who_needs_fut).await;

			let mut need_nodes = vec![];
			for (node, needed) in who.iter().zip(who_needs_resps.into_iter()) {
				match needed.err_context("NeedBlockQuery RPC")? {
					BlockRpc::NeedBlockReply(needed) => {
						if needed {
							need_nodes.push(*node);
						}
					}
					_ => {
						return Err(Error::Message(
							"Unexpected response to NeedBlockQuery RPC".to_string(),
						));
					}
				}
			}

			if !need_nodes.is_empty() {
				trace!(
					"Block {:?} needed by {} nodes, sending",
					hash,
					need_nodes.len()
				);

				let put_block_message = self.read_block(hash).await.err_context("PutBlock RPC")?;
				self.system
					.rpc
					.try_call_many(
						&self.endpoint,
						&need_nodes[..],
						put_block_message,
						RequestStrategy::with_priority(PRIO_BACKGROUND)
							.with_quorum(need_nodes.len())
							.with_timeout(BLOCK_RW_TIMEOUT),
					)
					.await?;
			}
			info!(
				"Deleting block {:?}, offload finished ({} / {})",
				hash,
				need_nodes.len(),
				who.len()
			);

			self.mutation_lock
				.lock()
				.await
				.delete_if_unneeded(hash, self)
				.await?;
		}

		if needed && !exists {
			// TODO find a way to not do this if they are sending it to us
			// Let's suppose this isn't an issue for now with the BLOCK_RW_TIMEOUT delay
			// between the RC being incremented and this part being called.
			let block_data = self.rpc_get_block(hash).await?;
			self.write_block(hash, &block_data[..]).await?;
		}

		Ok(())
	}

	async fn for_each_file<F, Fut, State>(
		&self,
		state: State,
		mut f: F,
		must_exit: &watch::Receiver<bool>,
	) -> Result<(), Error>
	where
		F: FnMut(State, Hash) -> Fut + Send,
		Fut: Future<Output = Result<State, Error>> + Send,
		State: Send,
	{
		self.for_each_file_rec(&self.data_dir, state, &mut f, must_exit)
			.await
			.map(|_| ())
	}

	fn for_each_file_rec<'a, F, Fut, State>(
		&'a self,
		path: &'a Path,
		mut state: State,
		f: &'a mut F,
		must_exit: &'a watch::Receiver<bool>,
	) -> BoxFuture<'a, Result<State, Error>>
	where
		F: FnMut(State, Hash) -> Fut + Send,
		Fut: Future<Output = Result<State, Error>> + Send,
		State: Send + 'a,
	{
		async move {
			let mut ls_data_dir = fs::read_dir(path).await?;
			while let Some(data_dir_ent) = ls_data_dir.next_entry().await? {
				if *must_exit.borrow() {
					break;
				}

				let name = data_dir_ent.file_name();
				let name = if let Ok(n) = name.into_string() {
					n
				} else {
					continue;
				};
				let ent_type = data_dir_ent.file_type().await?;

				if name.len() == 2 && hex::decode(&name).is_ok() && ent_type.is_dir() {
					state = self
						.for_each_file_rec(&data_dir_ent.path(), state, f, must_exit)
						.await?;
				} else if name.len() == 64 {
					let hash_bytes = if let Ok(h) = hex::decode(&name) {
						h
					} else {
						continue;
					};
					let mut hash = [0u8; 32];
					hash.copy_from_slice(&hash_bytes[..]);
					state = f(state, hash.into()).await?;
				}
			}
			Ok(state)
		}
		.boxed()
	}
}

#[async_trait]
impl EndpointHandler<BlockRpc> for BlockManager {
	async fn handle(
		self: &Arc<Self>,
		message: &BlockRpc,
		_from: NodeID,
	) -> Result<BlockRpc, Error> {
		match message {
			BlockRpc::PutBlock(m) => self.write_block(&m.hash, &m.data).await,
			BlockRpc::GetBlock(h) => self.read_block(h).await,
			BlockRpc::NeedBlockQuery(h) => self.need_block(h).await.map(BlockRpc::NeedBlockReply),
			_ => Err(Error::BadRpc("Unexpected RPC message".to_string())),
		}
	}
}

struct BlockStatus {
	exists: bool,
	needed: bool,
}

impl BlockManagerLocked {
	async fn check_block_status(
		&self,
		hash: &Hash,
		mgr: &BlockManager,
	) -> Result<BlockStatus, Error> {
		let path = mgr.block_path(hash);

		let exists = fs::metadata(&path).await.is_ok();
		let needed = mgr.get_block_rc(hash)? > 0;

		Ok(BlockStatus { exists, needed })
	}

	async fn write_block(
		&self,
		hash: &Hash,
		data: &[u8],
		mgr: &BlockManager,
	) -> Result<BlockRpc, Error> {
		let mut path = mgr.block_dir(hash);
		fs::create_dir_all(&path).await?;

		path.push(hex::encode(hash));
		if fs::metadata(&path).await.is_ok() {
			return Ok(BlockRpc::Ok);
		}

		let mut path2 = path.clone();
		path2.set_extension("tmp");
		let mut f = fs::File::create(&path2).await?;
		f.write_all(data).await?;
		drop(f);

		fs::rename(path2, path).await?;

		Ok(BlockRpc::Ok)
	}

	async fn move_block_to_corrupted(&self, hash: &Hash, mgr: &BlockManager) -> Result<(), Error> {
		warn!(
			"Block {:?} is corrupted. Renaming to .corrupted and resyncing.",
			hash
		);
		let path = mgr.block_path(hash);
		let mut path2 = path.clone();
		path2.set_extension("corrupted");
		fs::rename(path, path2).await?;
		mgr.put_to_resync(hash, Duration::from_millis(0))?;
		Ok(())
	}

	async fn delete_if_unneeded(&self, hash: &Hash, mgr: &BlockManager) -> Result<(), Error> {
		let BlockStatus { exists, needed } = self.check_block_status(hash, mgr).await?;

		if exists && !needed {
			let path = mgr.block_path(hash);
			fs::remove_file(path).await?;
		}
		Ok(())
	}
}

fn u64_from_be_bytes<T: AsRef<[u8]>>(bytes: T) -> u64 {
	assert!(bytes.as_ref().len() == 8);
	let mut x8 = [0u8; 8];
	x8.copy_from_slice(bytes.as_ref());
	u64::from_be_bytes(x8)
}
