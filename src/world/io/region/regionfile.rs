// TODO: Remove this when you no longer want to silence the warnings.

use std::{
	io::{
		BufReader, BufWriter,
		Read, Write,
		Seek, SeekFrom, Cursor,
	},
	fs::{
		File,
	},
	path::{
		Path,
	},
};

use flate2::{
	write::ZlibEncoder,
	read::{
		GzDecoder,
		ZlibDecoder,
	},
	Compression,
};

use crate::{
	McResult, McError,
	ioext::*,
};

use super::{
	prelude::*,
	{required_sectors, pad_size},
};

/// A construct for working with RegionFiles.
/// Allows for reading and writing data from a RegionFile.
pub struct RegionFile {
	header: RegionHeader,
	sector_manager: SectorManager,
	/// This file handle is for both reading and writing.
	file_handle: File,
	/// Because the write size of a value sometimes can't quite be known until
	/// after it has been written, it will be helpful to have a buffer to write
	/// to before writing to the file. This will allow us to know exactly how
	/// many 4KiB blocks are needed to write this data so that a sector can be
	/// allocated.
	write_buf: Cursor<Vec<u8>>,
}

impl RegionFile {
	/// Makes sure that the file located at the path is a valid region file. (must exist and have a size >= 8192).
	fn is_valid_region_file<P: AsRef<Path>>(path: P) -> McResult<bool> {
		let path = path.as_ref();
		// Must be file and the (length + 1) of the file must be large enough
		// to fit the header (8192 bytes). There doesn't need to be any
		// data beyond the header.
		Ok(path.is_file() && path.metadata()?.len() >= (4096*2))
	}

	pub fn sectors(&self) -> &SectorTable {
		&self.header.sectors
	}

	pub fn timestamps(&self) -> &TimestampTable {
		&self.header.timestamps
	}

	pub fn get_sector<C: Into<RegionCoord>>(&self, coord: C) -> RegionSector {
		let coord: RegionCoord = coord.into();
		self.header.sectors[coord.index()]
	}
	
	pub fn get_timestamp<C: Into<RegionCoord>>(&self, coord: C) -> Timestamp {
		let coord: RegionCoord = coord.into();
		self.header.timestamps[coord.index()]
	}

	/// Creates a new [RegionFile] object, opening or creating a Minecraft region file at the given path.
	pub fn new<P: AsRef<Path>>(path: P) -> McResult<Self> {
		let path = path.as_ref();
		if RegionFile::is_valid_region_file(path)? {
			let mut file_handle = File::options().write(true).read(true).open(path)?;
			let header = {				
				let mut temp_reader = BufReader::new((&mut file_handle).take(4096*2));
				RegionHeader::read_from(&mut temp_reader)?
			};
			let sector_manager = SectorManager::from(header.sectors.iter());
			Ok(Self {
				header,
				sector_manager,
				file_handle,
				write_buf: Cursor::new(Vec::with_capacity(4096*2)),
			})
		} else {
			// Create region file with empty header.
			let mut file_handle = File::options().write(true).read(true).create(true).open(path)?;
			// Write empty header.
			{
				let mut temp_writer = BufWriter::new(&mut file_handle);
				temp_writer.write_zeroes(4096*2)?;
			}
			let header = RegionHeader::default();
			let sector_manager = SectorManager::new();
			Ok(Self {
				header,
				sector_manager,
				file_handle,
				write_buf: Cursor::new(Vec::with_capacity(4096*2)),
			})
		}
	}

	/// Writes data to the region file at the specified coordinate and returns the [RegionSector] where it was written. This will also update the header (but will not update the timestamp).
	pub fn write_data<C: Into<RegionCoord>, T: Writable>(&mut self, coord: C, compression: Compression, value: &T) -> McResult<RegionSector> {
		let coord: RegionCoord = coord.into();
		// Clear the write_buf to prepare it for writing.
		self.write_buf.get_mut().clear();
		// Gotta write 5 bytes to the buffer so that there's room for the length and the compression scheme.
		// To kill two birds with one stone, I'll write all 2s so that I don't have to go back and write the
		// compression scheme after writing the length.
		self.write_buf.write_all(&[2u8; 5])?;
		// TODO: Allow tweaking the compression.
		// Now we'll write the data to the compressor.
		let mut encoder = ZlibEncoder::new(&mut self.write_buf, compression);
		value.write_to(&mut encoder)?;
		encoder.finish()?;
		// Get the length of the written data by getting the length of the buffer and subtracting 5 (for
		// the bytes that were pre-written in a previous step)
		let length = self.write_buf.get_ref().len() - 5;
		// Get sectors required to accomodate the buffer.
		// + 5 because you need to add the (length_bytes + CompressionScheme)
		let required_sectors = required_sectors((length + 5) as u32);
		// If there is an overflow, return an error because there's no way to write it to the file.
		if required_sectors > 255 {
			return Err(McError::ChunkTooLarge);
		}
		// Write pad zeroes
		// + 5 because you need to add the (length_bytes + CompressionScheme)
		let pad_bytes = pad_size((length + 5) as u64);
		self.write_buf.write_zeroes(pad_bytes)?;
		// Seek back to the beginning to write the length.
		self.write_buf.set_position(0);
		// Add 1 to the length because the specification requires that the compression scheme is included in the length for some reason.
		self.write_buf.write_value((length + 1) as u32)?;
		// Allocation
		let old_sector = self.header.sectors[coord.index()];
		let new_sector = if required_sectors == (old_sector.sector_count() as u32) {
			// If the data written takes up the same number of sectors as the old sector, then we can just use the old one.
			old_sector
		} else if required_sectors < (old_sector.sector_count() as u32) {
			// If the required sectors are LESS than the old_sector's size, then we don't need to allocate a new sector,
			// we can split our new one off from the old one and then free the old sector.
			// It's safe to unwrap here because of the check in the if expression.
			let (new_sector, old_sector) = old_sector.split_left(required_sectors as u8).unwrap();
			self.sector_manager.free(old_sector);
			new_sector
		} else {
			// We can't do anything with the old sector, so we'll just free it and allocate a new one.
			// Thankfully I wrote this special nethod to do just that.
			self.sector_manager.reallocate_err(old_sector, required_sectors as u8)?
		};
		// Writing to file
		self.file_handle.seek(SeekFrom::Start(new_sector.offset()))?;
		self.file_handle.write_all(self.write_buf.get_ref().as_slice())?;
		// Apply sector changes to the header table both in memory and in the file.
		self.header.sectors[coord.index()] = new_sector;
		// Seek to where sector is stored in the header and write the sector.
		let seek_offset = coord.index() * 4;
		self.file_handle.seek(SeekFrom::Start(seek_offset as u64))?;
		self.file_handle.write_value(new_sector)?;
		// I'm pretty sure that flush() doesn't do anything, but I'll put it here just in case.
		self.file_handle.flush()?;
		Ok(new_sector)
	}


	/// Writes data to the region file with a timestamp and returns the [RegionSector] where it was written.
	pub fn write_timestamped<C: Into<RegionCoord>, T: Writable, Ts: Into<Timestamp>>(&mut self, coord: C, compression: Compression, value: &T, timestamp: Ts) -> McResult<RegionSector> {
		let coord: RegionCoord = coord.into();
		let allocation = self.write_data(coord, compression, value)?;
		let timestamp: Timestamp = timestamp.into();
		self.header.timestamps[coord.index()] = timestamp;
		// Write the timestamp to the file.
		self.file_handle.seek(SeekFrom::Start((coord.index() * 4 + 4096) as u64))?;
		self.file_handle.write_value(timestamp)?;
		// I'm pretty sure that flush() doesn't do anything, but I'll put it here just in case.
		self.file_handle.flush()?;
		Ok(allocation)
	}

	/// Writes data to the region file with the `utc_now` timestamp and returns the [RegionSector] where it was written.
	pub fn write_with_utcnow<C: Into<RegionCoord>, T: Writable>(&mut self, coord: C, compression: Compression, value: &T) -> McResult<RegionSector> {
		self.write_timestamped(coord, compression, value, Timestamp::utc_now())
	}

	/// Reads data from the region file.
	pub fn read_data<C: Into<RegionCoord>, T: Readable>(&mut self, coord: C) -> McResult<T> {
		let coord: RegionCoord = coord.into();
		let sector = self.header.sectors[coord.index()];
		if sector.is_empty() {
			return Err(McError::ChunkNotFound);
		}
		self.file_handle.seek(SeekFrom::Start(sector.offset()))?;
		let mut reader = BufReader::new(&mut self.file_handle);
		let length: u32 = reader.read_value()?;
		if length == 0 {
			return Err(McError::ChunkNotFound);
		}
		let scheme: CompressionScheme = reader.read_value()?;
		match scheme {
			CompressionScheme::GZip => {
				let mut decoder = GzDecoder::new(reader.take((length - 1) as u64));
				T::read_from(&mut decoder)
			},
			CompressionScheme::ZLib => {
				let mut decoder = ZlibDecoder::new(reader.take((length - 1) as u64));
				T::read_from(&mut decoder)
			},
			CompressionScheme::Uncompressed => {
				T::read_from(&mut reader.take((length - 1) as u64))
			},
		}
	}
}