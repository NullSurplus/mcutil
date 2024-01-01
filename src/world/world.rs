/*

*/
#![allow(unused)]

use std::{collections::HashMap, path::{PathBuf, Path}, marker::PhantomData, sync::{Arc, Mutex}, ops::Rem};

use crate::{McResult, McError, nbt::tag::NamedTag};

use super::{
	blockregistry::BlockRegistry,
	blockstate::*,
	chunk::{Chunk, decode_chunk},
	io::region::{
		RegionFile,
		coord::RegionCoord,
		regionfile::{
			RegionManager,
		},
	},
};
use crate::math::coord::*;

pub trait ChunkManager: Sized {
	fn create<P: AsRef<Path>>(directory: P) -> McResult<Self>;
	fn load_chunk(&mut self, block_registry: &mut BlockRegistry, coord: WorldCoord) -> McResult<()>;
	fn save_chunk(&mut self, block_registry: &BlockRegistry, coord: WorldCoord) -> McResult<()>;
	fn save_all(&mut self, block_registry: &BlockRegistry) -> McResult<()>;
	fn unload_chunk(&mut self, coord: WorldCoord) -> McResult<()>;

	fn get_block_id(&self, block_registry: &BlockRegistry, coord: BlockCoord) -> McResult<Option<u32>>;
	fn get_block_state(&self, block_registry: &BlockRegistry, coord: BlockCoord) -> McResult<Option<BlockState>>;
	fn set_block_id(&mut self, block_registry: &mut BlockRegistry, coord: BlockCoord, id: u32) -> McResult<()>;
	fn set_block_state(&mut self, block_registry: &mut BlockRegistry, coord: BlockCoord, state: BlockState) -> McResult<()>;
}

pub struct JavaChunkManager {
	pub chunks: HashMap<WorldCoord, Arc<Mutex<Chunk>>>,
	pub regions: HashMap<WorldCoord, Arc<Mutex<RegionFile>>>,
	pub directory: PathBuf,
}

#[inline(always)]
fn make_arcmutex<T>(value: T) -> Arc<Mutex<T>> {
	Arc::new(Mutex::new(value))
}

impl JavaChunkManager {
	fn load_region(&mut self, coord: WorldCoord) -> McResult<Arc<Mutex<RegionFile>>> {
		if !self.regions.contains_key(&coord) {
			let region_dir = self.directory.join(match coord.dimension {
				Dimension::Overworld => "region",
				Dimension::Nether => todo!(),
				Dimension::Other(_) => todo!(),
			});
			let file_path = format!("r.{}.{}.mca", coord.x, coord.z);
			let file_path = region_dir.join(file_path);
			let region_file = if file_path.is_file() {
				make_arcmutex(RegionFile::open(file_path)?)
			} else {
				// If the file doesn't exist, we'll create a region file.
				make_arcmutex(RegionFile::create(file_path)?)
			};
			self.regions.insert(coord, region_file.clone());
			Ok(region_file)
		} else {
			Ok(self.regions.get(&coord).unwrap().clone())
		}
	}

	pub fn get_loaded_chunk(&self, coord: WorldCoord) -> Option<Arc<Mutex<Chunk>>> {
		if let Some(chunk) = self.chunks.get(&coord) {
			Some(chunk.clone())
		} else {
			None
		}
	}
}

impl ChunkManager for JavaChunkManager {
	fn create<P: AsRef<Path>>(directory: P) -> McResult<Self> {
		let directory = directory.as_ref().to_owned();
		if directory.is_dir() {
			Ok(Self {
				directory,
				chunks: HashMap::new(),
				regions: HashMap::new(),
			})
		} else {
			Err(McError::WorldDirectoryNotFound(directory))
		}
	}

	fn load_chunk(&mut self, block_registry: &mut BlockRegistry, coord: WorldCoord) -> McResult<()> {
		let region_coord = coord.region_coord();
		let (chunk_x, chunk_z) = (coord.x.rem_euclid(32), coord.z.rem_euclid(32));
		let region_file = self.load_region(region_coord)?;
		if let Ok(mut region) = region_file.lock() {
			let chunk_tag: NamedTag = region.read_data::<_,NamedTag>((chunk_x, chunk_z))?;
			let chunk = make_arcmutex(decode_chunk(block_registry, chunk_tag.tag)?);
			self.chunks.insert(coord, chunk);
		}
		Ok(())
	}

	fn save_chunk(&mut self, block_registry: &BlockRegistry, coord: WorldCoord) -> McResult<()> {
		let mut region_file = self.load_region(coord.region_coord())?;
		if let Ok(mut region) = region_file.lock() {
			if let Some(chunk) = self.chunks.get(&coord) {
				if let Ok(chunk) = chunk.lock() {
					let nbt = chunk.to_nbt(block_registry);
					let (x, z) = coord.xz();
					let x = x.rem_euclid(32);
					let z = z.rem_euclid(32);
					region.write_with_utcnow((x, z), &NamedTag::new(nbt))?;
				}
			}
		}
		Ok(())
	}

	fn save_all(&mut self, block_registry: &BlockRegistry) -> McResult<()> {
		todo!()
	}

	fn unload_chunk(&mut self, coord: WorldCoord) -> McResult<()> {
		self.chunks.remove(&coord);
		Ok(())
	}

	fn get_block_id(&self, block_registry: &BlockRegistry, coord: BlockCoord) -> McResult<Option<u32>> {
		let chunk_coord = coord.chunk_coord();
		if let Some(chunk) = self.chunks.get(&chunk_coord) {
			if let Ok(chunk) = chunk.lock() {
				return Ok(chunk.get_block_id(coord.xyz()));
			}
		}
		Ok(None)
	}

	fn get_block_state(&self, block_registry: &BlockRegistry, coord: BlockCoord) -> McResult<Option<BlockState>> {
		if let Some(id) = self.get_block_id(block_registry, coord)? {
			return Ok(block_registry.get(id));
		}
		Ok(None)
	}

	fn set_block_id(&mut self, block_registry: &mut BlockRegistry, coord: BlockCoord, id: u32) -> McResult<()> {
		let chunk_coord = coord.chunk_coord();
		if let Some(chunk) = self.chunks.get_mut(&chunk_coord) {
			if let Ok(mut chunk) = chunk.lock() {
				chunk.set_block_id(coord.xyz(), id);
			}
		}
		Ok(())
	}

	fn set_block_state(&mut self, block_registry: &mut BlockRegistry, coord: BlockCoord, state: BlockState) -> McResult<()> {
		let id = block_registry.register(state);
		self.set_block_id(block_registry, coord, id);
		Ok(())
	}
}

pub struct JavaWorld<M: ChunkManager> {
	pub block_registry: BlockRegistry,
	pub chunk_manager: M,
	directory: PathBuf,
}

impl<M: ChunkManager> JavaWorld<M> {
	pub fn open<P: AsRef<Path>>(directory: P) -> McResult<Self> {
		let directory = directory.as_ref().to_owned();
		if directory.is_dir() {
			Ok(Self {
				block_registry: BlockRegistry::with_air(),
				chunk_manager: M::create(&directory)?,
				directory,
			})
		} else {
			Err(McError::WorldDirectoryNotFound(directory))
		}
	}

	pub fn save(&mut self) -> McResult<()> {
		todo!()
	}
	
	pub fn load_chunk(&mut self, coord: WorldCoord) -> McResult<()> {
		self.chunk_manager.load_chunk(&mut self.block_registry, coord)
	}

	pub fn save_chunk(&mut self, coord: WorldCoord) -> McResult<()> {
		self.chunk_manager.save_chunk(&mut self.block_registry, coord)
	}

	pub fn save_all(&mut self, block_registry: &BlockRegistry) -> McResult<()> {
		self.chunk_manager.save_all(block_registry)
	}

	pub fn unload_chunk(&mut self, coord: WorldCoord) -> McResult<()> {
		self.chunk_manager.unload_chunk(coord)
	}
	
	pub fn get_block_id(&self, coord: BlockCoord) -> McResult<Option<u32>> {
		self.chunk_manager.get_block_id(&self.block_registry, coord)
	}

	pub fn get_block_state(&self, coord: BlockCoord) -> McResult<Option<BlockState>> {
		self.chunk_manager.get_block_state(&self.block_registry, coord)
	}

	pub fn set_block_id(&mut self, coord: BlockCoord, id: u32) -> McResult<()> {
		self.chunk_manager.set_block_id(&mut self.block_registry, coord, id)
	}

	pub fn set_block_state(&mut self, coord: BlockCoord, state: BlockState) -> McResult<()> {
		self.chunk_manager.set_block_state(&mut self.block_registry, coord, state)
	}
}

type ArcChunk = Arc<Mutex<Chunk>>;
type ArcRegion = Arc<Mutex<RegionFile>>;

pub struct VirtualJavaWorld {
	pub block_registry: BlockRegistry,
	pub chunks: HashMap<WorldCoord, ArcChunk>,
	pub regions: HashMap<WorldCoord, ArcRegion>,
	pub directory: PathBuf,
}

impl VirtualJavaWorld {
	pub fn new(directory: impl AsRef<Path>) -> Self {
		Self {
			block_registry: BlockRegistry::new(),
			chunks: HashMap::new(),
			regions: HashMap::new(),
			directory: directory.as_ref().to_owned(),
		}
	}

	pub fn get_region_directory(&self, dimension: Dimension) -> PathBuf {
		self.directory.join(match dimension {
			Dimension::Overworld => "region",
			Dimension::Nether => "poi",
			Dimension::Other(_) => todo!(),
		})
	}

	pub fn load_region(&mut self, coord: WorldCoord) -> McResult<ArcRegion> {
		if let Some(region) = self.regions.get(&coord) {
			Ok(region.clone())
		} else {
			let regiondir = self.get_region_directory(coord.dimension);
			let regname = format!("r.{}.{}.mca", coord.x, coord.z);
			let regfilepath = regiondir.join(regname);
			let regionfile = make_arcmutex(RegionFile::open_or_create(regfilepath)?);
			self.regions.insert(coord, regionfile.clone());
			Ok(regionfile)
		}
	}

	pub fn load_chunk(&mut self, coord: WorldCoord) -> McResult<ArcChunk> {
		let region = self.load_region(coord.region_coord())?;
		let regionlock = region.lock();
		if let Ok(mut regionfile) = regionlock {
			let root = regionfile.read_data::<_, NamedTag>(coord.xz())?;
			let chunk = make_arcmutex(decode_chunk(&mut self.block_registry, root.tag)?);
			self.chunks.insert(coord, chunk.clone());
			Ok(chunk)
		} else {
			McError::custom("Failed to lock region file.")
		}
	}

	pub fn get_chunk(&self, coord: WorldCoord) -> Option<ArcChunk> {
		if let Some(chunk) = self.chunks.get(&coord) {
			Some(chunk.clone())
		} else {
			None
		}
	}

	pub fn save_chunk(&mut self, coord: WorldCoord) -> McResult<()> {
		if let Some(chunk) = self.chunks.get(&coord) {
			let chunk = chunk.clone();
			let chunklock = chunk.lock();
			if let Ok(chunk) = chunklock {
				let nbt = chunk.to_nbt(&self.block_registry);
				let region = self.load_region(coord.region_coord())?;
				let regionlock = region.lock();
				if let Ok(mut regionfile) = regionlock {
					let root = NamedTag::new(nbt);
					regionfile.write_with_utcnow(coord.xz(), &root)?;
					return Ok(())
				}
			}
			McError::custom("Failed to write chunk to file.")
		} else {
			Ok(())
		}
	}

	pub fn unload_chunk(&mut self, coord: WorldCoord) -> Option<ArcChunk> {
		self.chunks.remove(&coord)
	}

	pub fn get_block_id(&self, coord: BlockCoord) -> Option<u32> {
		if let Some(chunk) = self.chunks.get(&coord.chunk_coord()) {
			if let Ok(chunk) = chunk.lock() {
				return chunk.get_block_id(coord.xyz());
			}
		}
		None
	}

	pub fn get_block_state(&self, coord: BlockCoord) -> Option<BlockState> {
		if let Some(id) = self.get_block_id(coord) {
			self.block_registry.get(id)
		} else {
			None
		}
	}

	pub fn set_block_id(&mut self, coord: BlockCoord, id: u32) -> Option<u32> {
		if let Some(chunk) = self.chunks.get(&coord.chunk_coord()) {
			if let Ok(mut chunk) = chunk.lock() {
				return chunk.set_block_id(coord.xyz(), id);
			}
		}
		None
	}

	pub fn set_block_state(&mut self, coord: BlockCoord, state: BlockState) -> Option<BlockState> {
		let id = self.block_registry.register(state);
		let old_id = self.set_block_id(coord, id);
		if let Some(id) = old_id {
			self.block_registry.get(id)
		} else {
			None
		}
	}
}

/*
World:
	chunks: HashMap<(i32, i32), ChunkType>
	
	Chunk Manager
		load_chunk
		save_chunk
	Block Registry
		register_block
		find_block
*/