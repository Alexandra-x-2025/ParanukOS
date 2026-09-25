//! UEFI Block I/O 协议的薄封装。
//!
//! 目前尚无调用方（属于后续块设备服务的基础件），但保持可编译、语义正确。

use uefi::proto::media::block::BlockIO;
use uefi::{Error, Result, Status};

/// 可读写的块设备抽象。
pub trait BlockDevice {
    /// 返回介质总字节数。
    fn size(&self) -> Result<u64>;

    /// 返回介质块大小（字节）。
    fn block_size(&self) -> Result<u32>;

    /// 从给定 LBA 读取数据到 `buffer`，返回实际读取的字节数。
    ///
    /// `buffer.len()` 必须是块大小的整数倍（UEFI 规范要求）。
    fn read_blocks(&self, lba: u64, buffer: &mut [u8]) -> Result<usize>;

    /// 从 `buffer` 写入数据到给定 LBA，返回实际写入的字节数。
    ///
    /// `buffer.len()` 必须是块大小的整数倍（UEFI 规范要求）。
    fn write_blocks(&mut self, lba: u64, buffer: &[u8]) -> Result<usize>;
}

/// 基于 UEFI `BlockIO` 协议的 [`BlockDevice`] 实现。
pub struct UefiBlockIo {
    protocol: BlockIO,
}

impl UefiBlockIo {
    /// 由 `BlockIO` 协议实例创建封装。
    pub fn new(protocol: BlockIO) -> Self {
        Self { protocol }
    }

    /// 返回介质 ID。
    pub fn media_id(&self) -> u32 {
        self.protocol.media().media_id()
    }

    /// 校验缓冲区长度是否为块大小的整数倍。
    fn check_buffer(buffer_len: usize, block_size: u32) -> Result<()> {
        if block_size == 0 || !buffer_len.is_multiple_of(block_size as usize) {
            return Err(Error::new(Status::INVALID_PARAMETER, ()));
        }
        Ok(())
    }
}

impl BlockDevice for UefiBlockIo {
    fn size(&self) -> Result<u64> {
        // BlockIOMedia 只提供 last_block（最后一个块的下标），
        // 因此字节数 = (last_block + 1) * block_size。
        let media = self.protocol.media();
        Ok((media.last_block() + 1) * u64::from(media.block_size()))
    }

    fn block_size(&self) -> Result<u32> {
        Ok(self.protocol.media().block_size())
    }

    fn read_blocks(&self, lba: u64, buffer: &mut [u8]) -> Result<usize> {
        let block_size = self.block_size()?;
        Self::check_buffer(buffer.len(), block_size)?;
        self.protocol.read_blocks(self.media_id(), lba, buffer)?;
        Ok(buffer.len())
    }

    fn write_blocks(&mut self, lba: u64, buffer: &[u8]) -> Result<usize> {
        let block_size = self.block_size()?;
        Self::check_buffer(buffer.len(), block_size)?;
        self.protocol.write_blocks(self.media_id(), lba, buffer)?;
        Ok(buffer.len())
    }
}
