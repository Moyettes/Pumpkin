use std::{fs, path::PathBuf, sync::Arc};

use dashmap::{DashMap, Entry};
use log::trace;
use num_traits::Zero;
use pumpkin_config::{advanced_config, chunk::ChunkFormat};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use tokio::{
    sync::{Mutex, Notify, RwLock, mpsc},
    task::{JoinHandle, JoinSet},
};
use tokio_util::task::TaskTracker;

use crate::{
    chunk::{
        ChunkData, ChunkParsingError, ChunkReadingError, ScheduledTick, TickPriority,
        format::{anvil::AnvilChunkFile, linear::LinearFile},
        io::{ChunkIO, LoadedData, chunk_file_manager::ChunkFileManager},
    },
    generation::{Seed, WorldGenerator, get_world_gen},
    lock::{LevelLocker, anvil::AnvilLevelLocker},
    world_info::{
        LevelData, WorldInfoError, WorldInfoReader, WorldInfoWriter,
        anvil::{AnvilLevelInfo, LEVEL_DAT_BACKUP_FILE_NAME, LEVEL_DAT_FILE_NAME},
    },
};

pub type SyncChunk = Arc<RwLock<ChunkData>>;

/// The `Level` module provides functionality for working with chunks within or outside a Minecraft world.
///
/// Key features include:
///
/// - **Chunk Loading:** Efficiently loads chunks from disk.
/// - **Chunk Caching:** Stores accessed chunks in memory for faster access.
/// - **Chunk Generation:** Generates new chunks on-demand using a specified `WorldGenerator`.
///
/// For more details on world generation, refer to the `WorldGenerator` module.
pub struct Level {
    pub seed: Seed,
    pub level_info: LevelData,
    world_info_writer: Arc<dyn WorldInfoWriter>,
    level_folder: LevelFolder,

    // Holds this level's spawn chunks, which are always loaded
    spawn_chunks: Arc<DashMap<Vector2<i32>, SyncChunk>>,

    // Chunks that are paired with chunk watchers. When a chunk is no longer watched, it is removed
    // from the loaded chunks map and sent to the underlying ChunkIO
    pub loaded_chunks: Arc<DashMap<Vector2<i32>, SyncChunk>>,
    chunk_watchers: Arc<DashMap<Vector2<i32>, usize>>,

    chunk_saver: Arc<dyn ChunkIO<Data = SyncChunk>>,
    world_gen: Arc<dyn WorldGenerator>,
    // Gets unlocked when dropped
    // TODO: Make this a trait
    _locker: Arc<AnvilLevelLocker>,
    block_ticks: Arc<Mutex<Vec<ScheduledTick>>>,
    /// Tracks tasks associated with this world instance
    tasks: TaskTracker,
    /// Notification that interrupts tasks for shutdown
    pub shutdown_notifier: Notify,
}

#[derive(Clone)]
pub struct LevelFolder {
    pub root_folder: PathBuf,
    pub region_folder: PathBuf,
}

impl Level {
    pub fn from_root_folder(root_folder: PathBuf) -> Self {
        // If we are using an already existing world we want to read the seed from the level.dat, If not we want to check if there is a seed in the config, if not lets create a random one
        let region_folder = root_folder.join("region");
        if !region_folder.exists() {
            std::fs::create_dir_all(&region_folder).expect("Failed to create Region folder");
        }
        let level_folder = LevelFolder {
            root_folder,
            region_folder,
        };

        // if we fail to lock, lets crash ???. maybe not the best solution when we have a large server with many worlds and one is locked.
        // So TODO
        let locker = AnvilLevelLocker::look(&level_folder).expect("Failed to lock level");

        // TODO: Load info correctly based on world format type
        let level_info = AnvilLevelInfo.read_world_info(&level_folder);
        if let Err(error) = &level_info {
            match error {
                // If it doesn't exist, just make a new one
                WorldInfoError::InfoNotFound => (),
                WorldInfoError::UnsupportedVersion(version) => {
                    log::error!("Failed to load world info!, {version}");
                    log::error!("{}", error);
                    panic!("Unsupported world data! See the logs for more info.");
                }
                e => {
                    panic!("World Error {}", e);
                }
            }
        } else {
            let dat_path = level_folder.root_folder.join(LEVEL_DAT_FILE_NAME);
            if dat_path.exists() {
                let backup_path = level_folder.root_folder.join(LEVEL_DAT_BACKUP_FILE_NAME);
                fs::copy(dat_path, backup_path).unwrap();
            }
        }

        let level_info = level_info.unwrap_or_default(); // TODO: Improve error handling
        log::info!(
            "Loading world with seed: {}",
            level_info.world_gen_settings.seed
        );

        let seed = Seed(level_info.world_gen_settings.seed as u64);
        let world_gen = get_world_gen(seed).into();

        let chunk_saver: Arc<dyn ChunkIO<Data = SyncChunk>> = match advanced_config().chunk.format {
            //ChunkFormat::Anvil => (Arc::new(AnvilChunkFormat), Arc::new(AnvilChunkFormat)),
            ChunkFormat::Linear => Arc::new(ChunkFileManager::<LinearFile>::default()),
            ChunkFormat::Anvil => Arc::new(ChunkFileManager::<AnvilChunkFile>::default()),
        };

        Self {
            seed,
            world_gen,
            world_info_writer: Arc::new(AnvilLevelInfo),
            level_folder,
            chunk_saver,
            spawn_chunks: Arc::new(DashMap::new()),
            loaded_chunks: Arc::new(DashMap::new()),
            chunk_watchers: Arc::new(DashMap::new()),
            level_info,
            _locker: Arc::new(locker),
            tasks: TaskTracker::new(),
            shutdown_notifier: Notify::new(),
            block_ticks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Spawns a task associated with this world. All tasks spawned with this method are awaited
    /// when the client. This means tasks should complete in a reasonable (no looping) amount of time.
    pub fn spawn_task<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn(task)
    }

    pub async fn shutdown(&self) {
        log::info!("Saving level...");

        self.shutdown_notifier.notify_waiters();
        self.tasks.close();
        log::debug!("Awaiting level tasks");
        self.tasks.wait().await;
        log::debug!("Done awaiting level tasks");

        // wait for chunks currently saving in other threads
        self.chunk_saver.block_and_await_ongoing_tasks().await;

        // save all chunks currently in memory
        let chunks_to_write = self
            .loaded_chunks
            .iter()
            .map(|chunk| (*chunk.key(), chunk.value().clone()))
            .collect::<Vec<_>>();
        self.loaded_chunks.clear();

        // TODO: I think the chunk_saver should be at the server level
        self.chunk_saver.clear_watched_chunks().await;
        self.write_chunks(chunks_to_write).await;

        // then lets save the world info
        let result = self
            .world_info_writer
            .write_world_info(self.level_info.clone(), &self.level_folder);

        // Lets not stop the overall save for this
        if let Err(err) = result {
            log::error!("Failed to save level.dat: {}", err);
        }
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.loaded_chunks.len()
    }

    pub async fn clean_up_log(&self) {
        self.chunk_saver.clean_up_log().await;
    }

    pub fn list_cached(&self) {
        for entry in self.loaded_chunks.iter() {
            log::debug!("In map: {:?}", entry.key());
        }
    }

    /// Marks chunks as "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was not watching
    /// before
    pub async fn mark_chunks_as_newly_watched(&self, chunks: &[Vector2<i32>]) {
        for chunk in chunks {
            log::trace!("{:?} marked as newly watched", chunk);
            match self.chunk_watchers.entry(*chunk) {
                Entry::Occupied(mut occupied) => {
                    let value = occupied.get_mut();
                    if let Some(new_value) = value.checked_add(1) {
                        *value = new_value;
                        //log::debug!("Watch value for {:?}: {}", chunk, value);
                    } else {
                        log::error!("Watching overflow on chunk {:?}", chunk);
                    }
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(1);
                }
            }
        }

        self.chunk_saver
            .watch_chunks(&self.level_folder, chunks)
            .await;
    }

    #[inline]
    pub async fn mark_chunk_as_newly_watched(&self, chunk: Vector2<i32>) {
        self.mark_chunks_as_newly_watched(&[chunk]).await;
    }

    /// Marks chunks no longer "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was watching before
    pub async fn mark_chunks_as_not_watched(&self, chunks: &[Vector2<i32>]) -> Vec<Vector2<i32>> {
        let mut chunks_to_clean = Vec::new();

        for chunk in chunks {
            log::trace!("{:?} marked as no longer watched", chunk);
            match self.chunk_watchers.entry(*chunk) {
                Entry::Occupied(mut occupied) => {
                    let value = occupied.get_mut();
                    *value = value.saturating_sub(1);

                    if *value == 0 {
                        occupied.remove_entry();
                        chunks_to_clean.push(*chunk);
                    }
                }
                Entry::Vacant(_) => {
                    // This can be:
                    // - Player disconnecting before all packets have been sent
                    // - Player moving so fast that the chunk leaves the render distance before it
                    // is loaded into memory
                }
            }
        }

        self.chunk_saver
            .unwatch_chunks(&self.level_folder, chunks)
            .await;
        chunks_to_clean
    }

    /// Returns whether the chunk should be removed from memory
    #[inline]
    pub async fn mark_chunk_as_not_watched(&self, chunk: Vector2<i32>) -> bool {
        !self.mark_chunks_as_not_watched(&[chunk]).await.is_empty()
    }

    pub async fn clean_chunks(self: &Arc<Self>, chunks: &[Vector2<i32>]) {
        // Care needs to be take here because of interweaving case:
        // 1) Remove chunk from cache
        // 2) Another player wants same chunk
        // 3) Load (old) chunk from serializer
        // 4) Write (new) chunk from serializer
        // Now outdated chunk data is cached and will be written later

        let chunks_with_no_watchers = chunks
            .iter()
            .filter_map(|pos| {
                // Only chunks that have no entry in the watcher map or have 0 watchers
                if self
                    .chunk_watchers
                    .get(pos)
                    .is_none_or(|count| count.is_zero())
                {
                    self.loaded_chunks
                        .get(pos)
                        .map(|chunk| (*pos, chunk.value().clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let level = self.clone();
        self.spawn_task(async move {
            let chunks_to_remove = chunks_with_no_watchers.clone();
            level.write_chunks(chunks_with_no_watchers).await;
            // Only after we have written the chunks to the serializer do we remove them from the
            // cache
            for (pos, _) in chunks_to_remove {
                let _ = level.loaded_chunks.remove_if(&pos, |_, _| {
                    // Recheck that there is no one watching
                    level
                        .chunk_watchers
                        .get(&pos)
                        .is_none_or(|count| count.is_zero())
                });
            }
        });
    }

    pub async fn clean_chunk(self: &Arc<Self>, chunk: &Vector2<i32>) {
        self.clean_chunks(&[*chunk]).await;
    }

    pub fn is_chunk_watched(&self, chunk: &Vector2<i32>) -> bool {
        self.chunk_watchers.get(chunk).is_some()
    }

    pub fn clean_memory(&self) {
        self.chunk_watchers.retain(|_, watcher| !watcher.is_zero());
        self.loaded_chunks
            .retain(|at, _| self.chunk_watchers.get(at).is_some());

        // if the difference is too big, we can shrink the loaded chunks
        // (1024 chunks is the equivalent to a 32x32 chunks area)
        if self.chunk_watchers.capacity() - self.chunk_watchers.len() >= 4096 {
            self.chunk_watchers.shrink_to_fit();
        }

        // if the difference is too big, we can shrink the loaded chunks
        // (1024 chunks is the equivalent to a 32x32 chunks area)
        if self.loaded_chunks.capacity() - self.loaded_chunks.len() >= 4096 {
            self.loaded_chunks.shrink_to_fit();
        }
    }

    pub async fn write_chunks(&self, chunks_to_write: Vec<(Vector2<i32>, SyncChunk)>) {
        if chunks_to_write.is_empty() {
            return;
        }
        let mut block_ticks = self.block_ticks.lock().await;

        for (coord, chunk) in &chunks_to_write {
            let mut chunk_data = chunk.write().await;
            chunk_data.block_ticks.clear();
            // Only keep ticks that are not saved in the chunk
            block_ticks.retain(|tick| {
                let (chunk_coord, _relative_coord) =
                    tick.block_pos.chunk_and_chunk_relative_position();
                if chunk_coord == *coord {
                    chunk_data.block_ticks.push(tick.clone());
                    false
                } else {
                    true
                }
            });
        }
        drop(block_ticks);

        let chunk_saver = self.chunk_saver.clone();
        let level_folder = self.level_folder.clone();

        trace!("Sending chunks to ChunkIO {:}", chunks_to_write.len());
        if let Err(error) = chunk_saver
            .save_chunks(&level_folder, chunks_to_write)
            .await
        {
            log::error!("Failed writing Chunk to disk {}", error);
        }
    }

    /// Initializes the spawn chunks to these chunks
    pub async fn read_spawn_chunks(self: &Arc<Self>, chunks: &[Vector2<i32>]) {
        let (send, mut recv) = mpsc::unbounded_channel();

        let fetcher = self.fetch_chunks(chunks, send);
        let handler = async {
            while let Some((chunk, _)) = recv.recv().await {
                let pos = chunk.read().await.position;
                self.spawn_chunks.insert(pos, chunk);
            }
        };

        let _ = tokio::join!(fetcher, handler);
        log::debug!("Read {} chunks as spawn chunks", chunks.len());
    }

    /// Reads/Generates many chunks in a world
    /// Note: The order of the output chunks will almost never be in the same order as the order of input chunks
    pub async fn fetch_chunks(
        self: &Arc<Self>,
        chunks: &[Vector2<i32>],
        channel: mpsc::UnboundedSender<(SyncChunk, bool)>,
    ) {
        if chunks.is_empty() {
            return;
        }

        let send_chunk =
            move |is_new: bool,
                  chunk: SyncChunk,
                  channel: &mpsc::UnboundedSender<(SyncChunk, bool)>| {
                let _ = channel
                    .send((chunk, is_new))
                    .inspect_err(|err| log::error!("unable to send chunk to channel: {}", err));
            };

        // First send all chunks that we have cached
        // We expect best case scenario to have all cached
        let mut remaining_chunks = Vec::new();
        for chunk in chunks {
            if let Some(chunk) = self.loaded_chunks.get(chunk) {
                send_chunk(false, chunk.value().clone(), &channel);
            } else if let Some(spawn_chunk) = self.spawn_chunks.get(chunk) {
                // Also clone the arc into the loaded chunks
                self.loaded_chunks
                    .insert(*chunk, spawn_chunk.value().clone());
                send_chunk(false, spawn_chunk.value().clone(), &channel);
            } else {
                remaining_chunks.push(*chunk);
            }
        }

        if remaining_chunks.is_empty() {
            return;
        }

        // These just pass data between async tasks, each of which do not block on anything, so
        // these do not need to hold a lot
        let (load_bridge_send, mut load_bridge_recv) =
            tokio::sync::mpsc::channel::<LoadedData<SyncChunk, ChunkReadingError>>(16);
        let (generate_bridge_send, mut generate_bridge_recv) = tokio::sync::mpsc::channel(16);

        let load_channel = channel.clone();
        let loaded_chunks = self.loaded_chunks.clone();
        let level_block_ticks = self.block_ticks.clone();
        let handle_load = async move {
            while let Some(data) = load_bridge_recv.recv().await {
                match data {
                    LoadedData::Loaded(chunk) => {
                        let position = chunk.read().await.position;

                        // Load the block ticks from the chunk
                        let block_ticks = chunk.read().await.block_ticks.clone();
                        let mut level_block_ticks = level_block_ticks.lock().await;
                        level_block_ticks.extend(block_ticks);
                        drop(level_block_ticks);

                        let value = loaded_chunks
                            .entry(position)
                            .or_insert(chunk)
                            .value()
                            .clone();
                        send_chunk(false, value, &load_channel);
                    }
                    LoadedData::Missing(pos) => generate_bridge_send
                        .send(pos)
                        .await
                        .expect("Failed to send position to generation handler"),
                    LoadedData::Error((pos, error)) => {
                        match error {
                            // this is expected, and is not an error
                            ChunkReadingError::ChunkNotExist
                            | ChunkReadingError::ParsingError(
                                ChunkParsingError::ChunkNotGenerated,
                            ) => {}
                            // this is an error, and we should log it
                            error => {
                                log::error!(
                                    "Failed to load chunk at {:?}: {} (regenerating)",
                                    pos,
                                    error
                                );
                            }
                        };

                        generate_bridge_send
                            .send(pos)
                            .await
                            .expect("Failed to send position to generation handler");
                    }
                }
            }
        };

        let loaded_chunks = self.loaded_chunks.clone();
        let world_gen = self.world_gen.clone();
        let handle_generate = async move {
            while let Some(pos) = generate_bridge_recv.recv().await {
                let loaded_chunks = loaded_chunks.clone();
                let world_gen = world_gen.clone();
                let channel = channel.clone();
                rayon::spawn(move || {
                    let result = loaded_chunks
                        .entry(pos)
                        .or_insert_with(|| {
                            // Avoid possible duplicating work by doing this within the dashmap lock
                            let generated_chunk = world_gen.generate_chunk(pos);
                            Arc::new(RwLock::new(generated_chunk))
                        })
                        .value()
                        .clone();

                    send_chunk(true, result, &channel);
                });
            }
        };

        let mut set = JoinSet::new();
        set.spawn(handle_load);
        set.spawn(handle_generate);

        self.chunk_saver
            .fetch_chunks(&self.level_folder, &remaining_chunks, load_bridge_send)
            .await;
        let _ = set.join_all().await;
    }

    pub fn try_get_chunk(
        &self,
        coordinates: Vector2<i32>,
    ) -> Option<dashmap::mapref::one::Ref<'_, Vector2<i32>, Arc<RwLock<ChunkData>>>> {
        self.loaded_chunks.try_get(&coordinates).try_unwrap()
    }

    pub async fn get_and_tick_block_ticks(&self) -> Vec<ScheduledTick> {
        let mut block_ticks = self.block_ticks.lock().await;
        let mut ticks = Vec::new();
        let mut remaining_ticks = Vec::new();
        for mut tick in block_ticks.drain(..) {
            tick.delay = tick.delay.saturating_sub(1);
            if tick.delay == 0 {
                ticks.push(tick);
            } else {
                remaining_ticks.push(tick);
            }
        }

        *block_ticks = remaining_ticks;
        ticks.sort_by_key(|tick| tick.priority);
        ticks
    }

    pub async fn is_block_tick_scheduled(&self, block_pos: &BlockPos, block_id: u16) -> bool {
        let block_ticks = self.block_ticks.lock().await;
        block_ticks
            .iter()
            .any(|tick| tick.block_pos == *block_pos && tick.target_block_id == block_id)
    }

    pub async fn schedule_block_tick(
        &self,
        block_id: u16,
        block_pos: BlockPos,
        delay: u16,
        priority: TickPriority,
    ) {
        let mut block_ticks = self.block_ticks.lock().await;
        block_ticks.push(ScheduledTick {
            block_pos,
            delay,
            priority,
            target_block_id: block_id,
        });
    }
}
