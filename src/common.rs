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
    max_len: usize,
) -> Result<String, anyhow::Error> {
    let size = read.read_u64().await? as usize;
    if size > max_len {
        bail!("String length {} exceeds maximum allowed {}", size, max_len);
    }
    let mut s = vec![0; size];
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
    max_len: usize,
    buf: Vec<u8>,
}

impl ResumableReadString {
    pub fn new(max_len: usize) -> Self {
        ResumableReadString {
            offset: 0,
            size: 0,
            max_len,
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
            if self.size as usize > self.max_len {
                bail!(
                    "String length {} exceeds maximum allowed {}",
                    self.size,
                    self.max_len
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ── write_string / read_string ──────────────────────────────────

    #[tokio::test]
    async fn write_read_string_round_trip() {
        let mut buf = Vec::new();
        write_string(&mut buf, "hello").await.unwrap();

        let mut cursor = Cursor::new(buf);
        let result = read_string(&mut cursor, 256).await.unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn write_read_string_empty() {
        let mut buf = Vec::new();
        write_string(&mut buf, "").await.unwrap();

        let mut cursor = Cursor::new(buf);
        let result = read_string(&mut cursor, 256).await.unwrap();
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn read_string_rejects_exceeding_max_len() {
        let mut buf = Vec::new();
        write_string(&mut buf, "toolong").await.unwrap();

        let mut cursor = Cursor::new(buf);
        let result = read_string(&mut cursor, 3).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[tokio::test]
    async fn read_string_accepts_exactly_max_len() {
        let mut buf = Vec::new();
        write_string(&mut buf, "abc").await.unwrap();

        let mut cursor = Cursor::new(buf);
        let result = read_string(&mut cursor, 3).await.unwrap();
        assert_eq!(result, "abc");
    }

    #[tokio::test]
    async fn write_read_string_with_unicode() {
        let mut buf = Vec::new();
        write_string(&mut buf, "héllo 🌍").await.unwrap();

        let mut cursor = Cursor::new(buf);
        let result = read_string(&mut cursor, 256).await.unwrap();
        assert_eq!(result, "héllo 🌍");
    }

    // ── write_status / check_status ─────────────────────────────────

    #[tokio::test]
    async fn write_check_status_ack() {
        let mut buf = Vec::new();
        write_status(&mut buf, StatusCode::Ack).await.unwrap();

        let mut cursor = Cursor::new(buf);
        check_status(&mut cursor).await.unwrap();
    }

    #[tokio::test]
    async fn check_status_rejects_non_ack() {
        let codes: [(u8, &str); 7] = [
            (1, "Invalid Secret"),
            (2, "File size differs"),
            (3, "Permission denied"),
            (4, "A client is already connected"),
            (5, "Hash algorithm is not supported"),
            (6, "Protocol version mismatch"),
            (7, "Invalid block size"),
        ];

        for (code_byte, expected_msg) in codes {
            let mut cursor = Cursor::new(vec![code_byte]);
            let err = check_status(&mut cursor).await.unwrap_err();
            assert!(
                err.to_string().contains(expected_msg),
                "Expected '{}' in error for code {}, got: {}",
                expected_msg,
                code_byte,
                err
            );
        }
    }

    #[tokio::test]
    async fn check_status_rejects_unknown_code() {
        let mut cursor = Cursor::new(vec![255u8]);
        let err = check_status(&mut cursor).await.unwrap_err();
        assert!(err.to_string().contains("Unknown status code"));
    }

    // ── ResumableWriteString / ResumableReadString round-trip ────────

    #[tokio::test]
    async fn resumable_write_read_string_round_trip() {
        let mut buf = Vec::new();
        let mut writer = ResumableWriteString::new("deltasync");
        writer.write_to(&mut buf).await.unwrap();

        let mut reader = ResumableReadString::new(256);
        let mut cursor = Cursor::new(buf);
        let result = reader.read_on(&mut cursor).await.unwrap();
        assert_eq!(result, "deltasync");
    }

    #[tokio::test]
    async fn resumable_write_read_string_empty() {
        let mut buf = Vec::new();
        let mut writer = ResumableWriteString::new("");
        writer.write_to(&mut buf).await.unwrap();

        let mut reader = ResumableReadString::new(256);
        let mut cursor = Cursor::new(buf);
        let result = reader.read_on(&mut cursor).await.unwrap();
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn resumable_read_string_rejects_exceeding_max_len() {
        let mut buf = Vec::new();
        let mut writer = ResumableWriteString::new("toolong");
        writer.write_to(&mut buf).await.unwrap();

        let mut reader = ResumableReadString::new(3);
        let mut cursor = Cursor::new(buf);
        let result = reader.read_on(&mut cursor).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[tokio::test]
    async fn resumable_read_string_multiple_sequential_reads() {
        let mut buf = Vec::new();
        ResumableWriteString::new("first")
            .write_to(&mut buf)
            .await
            .unwrap();
        ResumableWriteString::new("second")
            .write_to(&mut buf)
            .await
            .unwrap();

        let mut reader = ResumableReadString::new(256);
        let mut cursor = Cursor::new(buf);
        assert_eq!(reader.read_on(&mut cursor).await.unwrap(), "first");
        assert_eq!(reader.read_on(&mut cursor).await.unwrap(), "second");
    }

    // ── Resumable write/read are wire-compatible with simple write/read

    #[tokio::test]
    async fn resumable_write_compatible_with_simple_read() {
        let mut buf = Vec::new();
        let mut writer = ResumableWriteString::new("compat");
        writer.write_to(&mut buf).await.unwrap();

        let mut cursor = Cursor::new(buf);
        let result = read_string(&mut cursor, 256).await.unwrap();
        assert_eq!(result, "compat");
    }

    #[tokio::test]
    async fn simple_write_compatible_with_resumable_read() {
        let mut buf = Vec::new();
        write_string(&mut buf, "compat").await.unwrap();

        let mut reader = ResumableReadString::new(256);
        let mut cursor = Cursor::new(buf);
        let result = reader.read_on(&mut cursor).await.unwrap();
        assert_eq!(result, "compat");
    }

    // ── ResumableWriteFileBlock / ResumableReadFileBlock round-trip ──

    #[tokio::test]
    async fn resumable_write_read_file_block_round_trip() {
        let data = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let block_size = 10;
        let file_size = 100;
        let offset = 20; // block at offset 20

        let (loaned, reclaimer) = crate::sync::loan(data);

        let mut buf = Vec::new();
        let mut writer = ResumableWriteFileBlock::new(offset, loaned);
        writer.write_to(&mut buf).await.unwrap();
        drop(reclaimer);

        let mut reader = ResumableReadFileBlock::new(block_size, file_size);
        let mut read_buf = Vec::new();
        let mut cursor = Cursor::new(buf);
        let read_offset = reader.read_on(&mut cursor, &mut read_buf).await.unwrap();

        assert_eq!(read_offset, offset);
        assert_eq!(read_buf, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    }

    #[tokio::test]
    async fn resumable_write_read_file_block_last_block_smaller() {
        let data = vec![1u8, 2, 3]; // last block, only 3 bytes
        let block_size = 10;
        let file_size = 93; // offset 90 + 3 = 93
        let offset = 90;

        let (loaned, reclaimer) = crate::sync::loan(data);

        let mut buf = Vec::new();
        let mut writer = ResumableWriteFileBlock::new(offset, loaned);
        writer.write_to(&mut buf).await.unwrap();
        drop(reclaimer);

        let mut reader = ResumableReadFileBlock::new(block_size, file_size);
        let mut read_buf = Vec::new();
        let mut cursor = Cursor::new(buf);
        let read_offset = reader.read_on(&mut cursor, &mut read_buf).await.unwrap();

        assert_eq!(read_offset, offset);
        assert_eq!(read_buf, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn resumable_read_file_block_multiple_sequential() {
        let block_size = 4;
        let file_size = 10; // blocks: [0..4], [4..8], [8..10]

        let mut wire = Vec::new();

        // Write two blocks
        let (loaned, reclaimer) = crate::sync::loan(vec![0xAA; 4]);
        ResumableWriteFileBlock::new(0, loaned)
            .write_to(&mut wire)
            .await
            .unwrap();
        drop(reclaimer);

        let (loaned, reclaimer) = crate::sync::loan(vec![0xBB; 2]);
        ResumableWriteFileBlock::new(8, loaned)
            .write_to(&mut wire)
            .await
            .unwrap();
        drop(reclaimer);

        let mut reader = ResumableReadFileBlock::new(block_size, file_size);
        let mut cursor = Cursor::new(wire);

        let mut read_buf = Vec::new();
        let off = reader.read_on(&mut cursor, &mut read_buf).await.unwrap();
        assert_eq!(off, 0);
        assert_eq!(read_buf, vec![0xAA; 4]);

        let off = reader.read_on(&mut cursor, &mut read_buf).await.unwrap();
        assert_eq!(off, 8);
        assert_eq!(read_buf, vec![0xBB; 2]);
    }

    // ── ResumableAsyncWriteAll ──────────────────────────────────────

    #[tokio::test]
    async fn resumable_async_write_all() {
        let mut buf = Vec::new();
        let mut writer = ResumableAsyncWriteAll::new(&mut buf);
        writer.write_all(b"hello world").await.unwrap();
        assert_eq!(buf, b"hello world");
    }

    #[tokio::test]
    async fn resumable_async_write_all_empty() {
        let mut buf = Vec::new();
        let mut writer = ResumableAsyncWriteAll::new(&mut buf);
        writer.write_all(b"").await.unwrap();
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn resumable_async_write_all_cancel_and_resume() {
        let data = b"abcdefghijklmnopqrstuvwxyz";

        // duplex(4): writer blocks after 4 bytes with no reader
        let (mut rx, mut tx) = tokio::io::duplex(4);
        let mut writer = ResumableAsyncWriteAll::new(&mut tx);

        tokio::select! {
            _ = writer.write_all(data) => panic!("should not complete with 4-byte buffer and no reader"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        // Spawn a reader that drains everything until EOF
        let reader_handle = tokio::spawn(async move {
            let mut all = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut rx, &mut all)
                .await
                .unwrap();
            all
        });

        // Resume from saved offset
        writer.write_all(data).await.unwrap();
        drop(writer);
        drop(tx);

        let received = reader_handle.await.unwrap();
        assert_eq!(received, data);
    }

    // ── ResumableReadString connection closed ───────────────────────

    #[tokio::test]
    async fn resumable_read_string_connection_closed_during_header() {
        let mut reader = ResumableReadString::new(256);
        // Only 3 bytes when 8 are needed for the length header
        let mut cursor = Cursor::new(vec![0u8, 0, 0]);
        let result = reader.read_on(&mut cursor).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Connection closed")
        );
    }

    #[tokio::test]
    async fn resumable_read_string_connection_closed_during_payload() {
        // Encode length = 10 in big-endian u64, but only provide 5 bytes of payload
        let mut data = vec![0u8, 0, 0, 0, 0, 0, 0, 10];
        data.extend_from_slice(b"short");

        let mut reader = ResumableReadString::new(256);
        let mut cursor = Cursor::new(data);
        let result = reader.read_on(&mut cursor).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Connection closed")
        );
    }

    // ── Cancel-safety: read_on/write_to survive tokio::select! ──────

    #[tokio::test]
    async fn resumable_read_string_cancel_during_header() {
        // Wire format for "hello": [0,0,0,0,0,0,0,5] + b"hello" = 13 bytes
        let wire = {
            let mut buf = Vec::new();
            write_string(&mut buf, "hello").await.unwrap();
            buf
        };

        let (mut tx, mut rx) = tokio::io::duplex(64);
        let mut reader = ResumableReadString::new(256);

        // Feed only 3 bytes of the 8-byte header — read_on will block
        tx.write_all(&wire[..3]).await.unwrap();

        tokio::select! {
            _ = reader.read_on(&mut rx) => panic!("should not complete with partial header"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        // reader consumed 3 bytes, offset is at 3. Feed the rest.
        tx.write_all(&wire[3..]).await.unwrap();

        // Resume — must complete correctly from where it left off
        let result = reader.read_on(&mut rx).await.unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn resumable_read_string_cancel_during_payload() {
        // Wire format for "hello world!": 8-byte header + 12 bytes payload = 20 bytes
        let wire = {
            let mut buf = Vec::new();
            write_string(&mut buf, "hello world!").await.unwrap();
            buf
        };

        let (mut tx, mut rx) = tokio::io::duplex(64);
        let mut reader = ResumableReadString::new(256);

        // Feed entire header + 4 bytes of payload (12 of 20 bytes)
        tx.write_all(&wire[..12]).await.unwrap();

        tokio::select! {
            _ = reader.read_on(&mut rx) => panic!("should not complete with partial payload"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        // Feed the remaining 8 bytes of payload
        tx.write_all(&wire[12..]).await.unwrap();

        let result = reader.read_on(&mut rx).await.unwrap();
        assert_eq!(result, "hello world!");
    }

    #[tokio::test]
    async fn resumable_write_string_cancel_and_resume() {
        // "hello world!" = 12 bytes payload, 20 bytes wire total
        let mut writer = ResumableWriteString::new("hello world!");

        // duplex(4): writer can push at most 4 bytes before blocking
        let (mut rx, mut tx) = tokio::io::duplex(4);

        // Cancel write_to after it has written some bytes and is blocked
        tokio::select! {
            _ = writer.write_to(&mut tx) => panic!("should not complete with 4-byte buffer and no reader"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        // Spawn a reader that drains everything until EOF
        let reader_handle = tokio::spawn(async move {
            let mut all = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut rx, &mut all)
                .await
                .unwrap();
            all
        });

        // Resume — writer continues from its saved offset, reader drains concurrently
        writer.write_to(&mut tx).await.unwrap();
        drop(tx);

        let received = reader_handle.await.unwrap();

        // Verify the full wire output matches what a fresh single-shot write produces
        let mut expected = Vec::new();
        ResumableWriteString::new("hello world!")
            .write_to(&mut expected)
            .await
            .unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn resumable_read_file_block_cancel_and_resume() {
        let block_size = 10;
        let file_size = 100;
        let offset = 30;
        let payload = vec![0xCC; 10];

        // Build wire data: 8-byte offset header + 10-byte payload = 18 bytes
        let wire = {
            let mut buf = Vec::new();
            let (loaned, reclaimer) = crate::sync::loan(payload.clone());
            ResumableWriteFileBlock::new(offset, loaned)
                .write_to(&mut buf)
                .await
                .unwrap();
            drop(reclaimer);
            buf
        };

        let (mut tx, mut rx) = tokio::io::duplex(64);
        let mut reader = ResumableReadFileBlock::new(block_size, file_size);
        let mut read_buf = Vec::new();

        // Feed only 5 bytes of the 8-byte offset header
        tx.write_all(&wire[..5]).await.unwrap();

        tokio::select! {
            _ = reader.read_on(&mut rx, &mut read_buf) => panic!("should not complete with partial header"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        // Feed the rest
        tx.write_all(&wire[5..]).await.unwrap();

        let read_offset = reader.read_on(&mut rx, &mut read_buf).await.unwrap();
        assert_eq!(read_offset, offset);
        assert_eq!(read_buf, payload);
    }

    #[tokio::test]
    async fn resumable_write_file_block_cancel_and_resume() {
        let offset = 40;
        let payload = vec![0xDD; 16];

        let (loaned, reclaimer) = crate::sync::loan(payload);
        let mut writer = ResumableWriteFileBlock::new(offset, loaned);

        // duplex(4): forces partial writes
        let (mut rx, mut tx) = tokio::io::duplex(4);

        tokio::select! {
            _ = writer.write_to(&mut tx) => panic!("should not complete with 4-byte buffer and no reader"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        let reader_handle = tokio::spawn(async move {
            let mut all = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut rx, &mut all)
                .await
                .unwrap();
            all
        });

        writer.write_to(&mut tx).await.unwrap();
        drop(tx);
        drop(reclaimer);

        let received = reader_handle.await.unwrap();

        // Verify by reading the wire data back
        let mut reader = ResumableReadFileBlock::new(16, 100);
        let mut read_buf = Vec::new();
        let mut cursor = Cursor::new(received);
        let read_offset = reader.read_on(&mut cursor, &mut read_buf).await.unwrap();
        assert_eq!(read_offset, offset);
        assert_eq!(read_buf, vec![0xDD; 16]);
    }
}
