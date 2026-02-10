use crate::sync::Loan;
use anyhow::{anyhow, bail};
use int_enum::IntEnum;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PROTOCOL_VERSION: u8 = 1;

#[repr(u8)]
#[derive(Debug, IntEnum)]
pub enum StatusCode {
    Ack = 0,
    InvalidSecret = 1,
    FileSizeDiffers = 2,
    PermissionDenied = 3,
    ClientAlreadyConnected = 4,
    UnknownHashAlgorithm = 5,
    UnsupportedProtocolVersion = 6,
    InvalidBlockSize = 7,
}

pub async fn write_string<T: AsyncWriteExt + std::marker::Unpin, S: Into<String>>(
    write: &mut T,
    s: S,
) -> Result<(), anyhow::Error> {
    let s = s.into();
    write.write_u64(s.len().try_into()?).await?;
    write.write_all(s.as_bytes()).await?;
    Ok(())
}

pub async fn read_string<T: AsyncReadExt + std::marker::Unpin>(
    read: &mut T,
) -> Result<String, anyhow::Error> {
    let size = read.read_u64().await?;
    let mut s = vec![0; size as usize];
    read.read_exact(s.as_mut_slice()).await?;
    Ok(String::from_utf8(s)?)
}

pub async fn write_status<T: AsyncWriteExt + std::marker::Unpin>(
    write: &mut T,
    status: StatusCode,
) -> Result<(), anyhow::Error> {
    write.write_u8(status.into()).await?;
    Ok(())
}

pub async fn check_status<T: AsyncReadExt + std::marker::Unpin>(
    read: &mut T,
) -> Result<(), anyhow::Error> {
    let status = StatusCode::try_from(read.read_u8().await?)
        .map_err(|_| anyhow::anyhow!("Unknown status code"))?;

    match status {
        StatusCode::Ack => Ok(()),
        StatusCode::InvalidSecret => Err(anyhow!("Invalid Secret")),
        StatusCode::FileSizeDiffers => Err(anyhow!("File size differs")),
        StatusCode::PermissionDenied => Err(anyhow!("Permission denied")),
        StatusCode::ClientAlreadyConnected => Err(anyhow!("A client is already connected")),
        StatusCode::UnknownHashAlgorithm => {
            Err(anyhow!("Hash algorithm is not supported on remote end"))
        }
        StatusCode::UnsupportedProtocolVersion => Err(anyhow!("Protocol version mismatch")),
        StatusCode::InvalidBlockSize => Err(anyhow!("Invalid block size")),
    }
}

/// Struct which allows to read a string similar to `read_string` in a select branch.
pub struct ResumableReadString {
    offset: usize,
    size: u64,
    buf: Vec<u8>,
}

impl ResumableReadString {
    pub fn new() -> Self {
        ResumableReadString {
            offset: 0,
            size: 0,
            buf: Vec::new(),
        }
    }

    pub async fn read_on<T: AsyncReadExt + std::marker::Unpin>(
        &mut self,
        read: &mut T,
    ) -> Result<String, anyhow::Error> {
        if self.size == 0 {
            self.buf.resize(8, 0);
            while self.offset < 8 {
                let n = read.read(&mut self.buf[self.offset..]).await?;
                if n == 0 {
                    bail!("Connection closed by peer");
                }
                self.offset += n;
            }
            for i in 0..8 {
                self.size <<= 8;
                self.size += self.buf[i] as u64;
            }
            self.buf.clear();
            self.buf.resize(self.size as usize, 0);
        }

        while self.offset < (self.size as usize + 8) {
            let n = read.read(&mut self.buf[(self.offset - 8)..]).await?;
            if n == 0 {
                bail!("Connection closed by peer");
            }
            self.offset += n;
        }

        // prepare for next string
        self.offset = 0;
        self.size = 0;

        Ok(String::from_utf8(std::mem::take(&mut self.buf))?)
    }
}

/// Struct which allows to write a string similar to `write_string` in a select branch.
pub struct ResumableWriteString {
    offset: usize,
    buf: Vec<u8>,
}

impl ResumableWriteString {
    pub fn new(src: &str) -> Self {
        let mut len = src.len();
        let mut vec = Vec::with_capacity(8 + len);
        vec.resize(8, 0);
        for i in (0..8).rev() {
            vec[i] = (len & 0xFF) as u8;
            len >>= 8;
        }
        vec.extend_from_slice(src.as_bytes());

        ResumableWriteString {
            offset: 0,
            buf: vec,
        }
    }

    pub async fn write_to<T: AsyncWriteExt + std::marker::Unpin>(
        &mut self,
        write: &mut T,
    ) -> Result<(), anyhow::Error> {
        while self.offset < self.buf.len() {
            let n = write.write(&self.buf[self.offset..]).await?;
            self.offset += n;
        }

        Ok(())
    }
}

pub struct ResumableWriteFileBlock {
    offset: usize,
    offsetlen: usize,
    offset_bytes: [u8; 8],
    bufoffset: usize,
    buf: Loan<Vec<u8>>,
    step: u8, // 0: init, 1: size, 2: payload
}

impl ResumableWriteFileBlock {
    /// Create a new `ResumableWriteFileBlock` for reading a block of the given file.
    /// Precondition: file pointer MUST be already set to `offset`.
    pub fn new(offset: usize, block_data: Loan<Vec<u8>>) -> Self {
        ResumableWriteFileBlock {
            offset,
            offsetlen: 0,
            offset_bytes: [0; 8],
            bufoffset: 0,
            buf: block_data,
            step: 0,
        }
    }

    pub async fn write_to<T: AsyncWriteExt + std::marker::Unpin>(
        &mut self,
        write: &mut T,
    ) -> Result<(), anyhow::Error> {
        if self.step == 0 {
            let mut offset: u64 = self.offset.try_into()?;

            for i in (0..8).rev() {
                self.offset_bytes[i] = (offset & 0xFF) as u8;
                offset >>= 8;
            }

            self.offsetlen = 8;
            self.step = 1;
        }

        if self.step == 1 {
            while self.offsetlen > 0 {
                let n = write.write(&self.offset_bytes[0..self.offsetlen]).await?;
                self.offset_bytes.copy_within(n.., 0);
                self.offsetlen -= n;
            }
            self.step = 2;
        }

        while self.bufoffset < self.buf.len() {
            let n = write.write(&self.buf[self.bufoffset..]).await?;
            self.bufoffset += n;
        }
        Ok(())
    }
}

pub struct ResumableReadFileBlock {
    offset: usize,
    block_size: usize,
    file_size: usize,
    buf_offset: usize,
}

impl ResumableReadFileBlock {
    pub fn new(block_size: usize, file_size: usize) -> Self {
        ResumableReadFileBlock {
            offset: 0,
            block_size,
            file_size,
            buf_offset: 0,
        }
    }

    pub async fn read_on<T: AsyncReadExt + std::marker::Unpin>(
        &mut self,
        read: &mut T,
        buf: &mut Vec<u8>,
    ) -> Result<usize, anyhow::Error> {
        if self.buf_offset < 8 {
            buf.resize(8, 0);
            while self.buf_offset < 8 {
                let n = read.read(&mut buf[self.buf_offset..]).await?;
                if n == 0 {
                    bail!("Connection closed by peer");
                }
                self.buf_offset += n;
            }
            for val in buf.iter().take(8) {
                self.offset <<= 8;
                self.offset += *val as usize;
            }

            let current_block_size = {
                let rem = self.file_size - self.offset;
                if rem < self.block_size {
                    rem
                } else {
                    self.block_size
                }
            };

            buf.clear();
            buf.resize(current_block_size, 0);
        }

        while self.buf_offset < (buf.len() + 8) {
            let n = read.read(&mut buf[(self.buf_offset - 8)..]).await?;
            if n == 0 {
                bail!("Connection closed by peer");
            }
            self.buf_offset += n;
        }

        let block_offset = self.offset;

        // prepare for next block
        self.offset = 0;
        self.buf_offset = 0;

        Ok(block_offset)
    }
}

pub struct ResumableAsyncWriteAll<'a, T: AsyncWriteExt> {
    write: &'a mut T,
    offset: usize,
}

impl<'a, T: AsyncWriteExt + std::marker::Unpin> ResumableAsyncWriteAll<'a, T> {
    pub fn new(write: &'a mut T) -> Self {
        ResumableAsyncWriteAll { write, offset: 0 }
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), anyhow::Error> {
        while self.offset < data.len() {
            let n = self.write.write(&data[self.offset..]).await?;
            self.offset += n;
        }

        Ok(())
    }
}
