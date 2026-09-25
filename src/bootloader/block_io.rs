use uefi::proto::media::block::{BlockIO, BlockIOMedia, Lba};
use uefi::Result;

/// Represents a block device that can be read from or written to.
pub trait BlockDevice {
    /// Returns the size of the media in bytes.
    fn size(&self) -> Result<u64>;

    /// Returns the block size of the media.
    fn block_size(&self) -> Result<u32>;

    /// Reads a number of blocks from the device starting at a given LBA into a buffer.
    fn read_blocks(&self, lba: u64, buffer: &mut [u8]) -> Result<usize>;

    /// Writes a number of blocks to the device starting at a given LBA from a buffer.
    fn write_blocks(&self, lba: u64, buffer: &[u8]) -> Result<usize>;
}

/// A concrete implementation of `BlockDevice` using the UEFI BlockIO protocol.
pub struct UefiBlockIo {
    protocol: BlockIO,
}

impl UefiBlockIo {
    /// Creates a new `UefiBlockIo` instance from a `BlockIO` protocol handle.
    pub fn new(protocol: BlockIO) -> Self {
        Self { protocol }
    }

    /// Returns the media ID for the device.
    pub fn media_id(&self) -> u32 {
        self.protocol.media().media_id
    }
}

impl BlockDevice for UefiBlockIo {
    fn size(&self) -> Result<u64> {
        let media = self.protocol.media();
        Ok(media.size as u64)
    }

    fn block_size(&self) -> Result<u32> {
        let media = self.protocol.media();
        Ok(media.block_size)
    }

    fn read_blocks(&self, lba: u64, buffer: &mut [u8]) -> Result<usize> {
        // Note: This is a simplified implementation. 
        // In a real bootloader, we'd need to handle block alignment and potential multi-block reads.
        let mut read_bytes = 0;
        for chunk in buffer.chunks_mut(4096) { // Assuming 4KB blocks for now as an example
            self.protocol.read_blocks(self.media_id(), Lba(lba), chunk)?;
            read_bytes += chunk.len();
            lba += (chunk.len() / self.block_size().unwrap() as usize) as u64;
        }
        Ok(read_bytes)
    }

    fn write_blocks(&self, lba: u64, buffer: &[u8]) -> Result<usize> {
        // Similar to read_blocks but for writing.
        let mut written_bytes = 0;
        for chunk in buffer.chunks(4096) {
            self.protocol.write_blocks(self.media_id(), Lba(lba), chunk)?;
            written_bytes += chunk.len();
            lba += (chunk.len() / self.block_size().unwrap() as usize) as u64;
        }
        Ok(written_bytes)
    }
}
