use crate::sync::{loan, Loan};
use crate::sync::{ordered_channel, OrderedReceiver};
use crate::HashAlgorithm;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

pub struct FileBlockData {
    pub hash: String,
    pub data: Loan<Vec<u8>>,
}

pub struct HashStream {
    rx: OrderedReceiver<(usize, FileBlockData)>,
    join_handles: Vec<std::thread::JoinHandle<()>>,
    error_rx: tokio::sync::watch::Receiver<Option<String>>,
}

fn hash_data(data: &[u8], algorithm: HashAlgorithm) -> String {
    use md5::{Digest, Md5};

    match algorithm {
        HashAlgorithm::CRC32 => hex::encode(crc32c::crc32c(data).to_le_bytes()),
        HashAlgorithm::MD5 => hex::encode(Md5::digest(data)),
    }
}

pub fn hash_file(
    path: impl AsRef<Path>,
    block_size: usize,
    workers: u8,
    algorithm: HashAlgorithm,
) -> Result<HashStream, anyhow::Error> {
    let path = {
        let mut p = PathBuf::new();
        p.push(path);
        p
    };

    let file_size = {
        let mut file = File::open(&path)?;
        file.seek(SeekFrom::End(0))? as usize
    };

    // let start the workers threads
    let mut join_handles = Vec::with_capacity(workers as usize);
    let (tx, rx) = ordered_channel();
    let (error_tx, error_rx) = tokio::sync::watch::channel(None);
    let error_tx = Arc::new(error_tx);

    for i in 0..(workers as usize) {
        let mut tx = tx.clone();
        let path = path.clone();
        let error_tx = Arc::clone(&error_tx);
        let handle = thread::spawn(move || {
            let mut offset = i * block_size;
            let mut block_idx = i;
            let mut file = {
                match File::open(&path) {
                    Ok(file) => file,
                    Err(e) => {
                        let _ = error_tx.send(Some(format!("Failed to open file: {}", e)));
                        tx.close();
                        return;
                    }
                }
            };
            if let Err(e) = file.seek(SeekFrom::Start(offset as u64)) {
                let _ = error_tx.send(Some(format!("Failed to seek: {}", e)));
                tx.close();
                return;
            }

            let mut buffer = vec![0; block_size];

            while offset < file_size {
                use std::io::Read;

                if error_tx.borrow().is_some() {
                    break;
                }

                let hash = {
                    let current_block_size = {
                        let rem = file_size - offset;
                        if rem < block_size {
                            rem
                        } else {
                            block_size
                        }
                    };
                    buffer.resize(current_block_size, 0);
                    let buf = &mut buffer;

                    match file.read_exact(buf) {
                        Ok(()) => {}
                        Err(e) => {
                            let _ = error_tx
                                .send(Some(format!("Failed to read block {}: {}", block_idx, e)));
                            tx.close();
                            break;
                        }
                    }

                    hash_data(buf, algorithm)
                };

                let (loaned, reclaimer) = loan(buffer);
                let mut block_data = FileBlockData::new(loaned);
                block_data.hash = hash;

                if tx.send(block_idx, (block_idx, block_data)).is_err() {
                    break; // receiver closed, indicating we do not want hash any longer
                }

                buffer = reclaimer.blocking_reclaim();

                match file.seek(SeekFrom::Current(
                    (((workers - 1) as usize) * block_size) as i64,
                )) {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = error_tx.send(Some(format!("Failed to seek to next block: {}", e)));
                        tx.close();
                        break;
                    }
                }
                offset += (workers as usize) * block_size;
                block_idx += workers as usize;
            }
        });
        join_handles.push(handle);
    }

    Ok(HashStream {
        rx,
        join_handles,
        error_rx,
    })
}

impl HashStream {
    pub async fn recv(&mut self) -> Result<Option<(usize, FileBlockData)>, anyhow::Error> {
        if let Some(ref err) = *self.error_rx.borrow_and_update() {
            anyhow::bail!("Hash worker failed: {}", err);
        }
        tokio::select! {
            value = self.rx.recv() => Ok(value),
            Ok(()) = self.error_rx.changed() => {
                let msg = self.error_rx.borrow_and_update().clone().unwrap();
                anyhow::bail!("Hash worker failed: {}", msg);
            },
        }
    }
}

impl std::ops::Drop for HashStream {
    fn drop(&mut self) {
        self.rx.close();
        let join_handles = std::mem::take(&mut self.join_handles);
        join_handles.into_iter().for_each(|t| {
            let _ = t.join();
        });
    }
}

impl FileBlockData {
    fn new(data: Loan<Vec<u8>>) -> Self {
        FileBlockData {
            hash: String::new(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn hashes() {
        use super::*;

        let mut hasher =
            hash_file("test/file_to_hash.txt", 10, 2, crate::HashAlgorithm::MD5).unwrap();

        let assert_hash = |file_block_data: FileBlockData, expected_hash| {
            assert_eq!(file_block_data.hash, expected_hash);
        };

        {
            let (idx, data) = hasher.recv().await.unwrap().unwrap();
            assert_eq!(idx, 0);
            assert_hash(data, "87569f2a32a30008939dbce402476f9e");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap().unwrap();
            assert_eq!(idx, 1);
            assert_hash(data, "4bb90b25dbd367acfb66910764a60669");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap().unwrap();
            assert_eq!(idx, 2);
            assert_hash(data, "2ae5379d67740e8bb6ce27404873efa5");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap().unwrap();
            assert_eq!(idx, 3);
            assert_hash(data, "330880debc47896dacf1b11aa46ada4a");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap().unwrap();
            assert_eq!(idx, 4);
            assert_hash(data, "a80a682a9206c851a4dd9d809a20e4c5");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap().unwrap();
            assert_eq!(idx, 5);
            assert_hash(data, "590f3733f7c58433d9077904b0f1a6ee");
        }

        assert!(hasher.recv().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dropped() {
        use super::*;

        let mut hasher =
            hash_file("test/file_to_hash.txt", 10, 2, crate::HashAlgorithm::CRC32).unwrap();

        let _ = hasher.recv().await.unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        // we do not read all and the senders should not cause deadlock because
        // they must not have sent any Loan to the channel (ordered_queue buffer is guaranteed to be empty)
        // So they will detect that rx is dropped on their next call to tx.send()
    }

    #[tokio::test]
    async fn no_deadlock_on_worker_io_error() {
        use super::*;
        use std::io::Write;

        // Create a 100-byte temp file
        let dir = std::env::temp_dir().join("deltasync_test_deadlock");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test_file.bin");
        {
            let mut f = std::fs::File::create(&file_path).unwrap();
            f.write_all(&[0xABu8; 100]).unwrap();
            f.flush().unwrap();
        }

        let mut hasher = hash_file(&file_path, 10, 2, crate::HashAlgorithm::CRC32).unwrap();

        // Read block 0 — hold the return value (keeps the Loan alive, blocking worker 0 on blocking_reclaim)
        let block0 = hasher.recv().await.unwrap().unwrap();

        // Read block 1 — drop it immediately (frees worker 1, which then blocks on condvar trying to send block 3)
        let _ = hasher.recv().await.unwrap().unwrap();

        // Truncate the file to 0 bytes
        std::fs::File::create(&file_path).unwrap();

        // Drop block 0's data — worker 0 unblocks, seeks to block 2 offset, read_exact fails (EOF on empty file)
        drop(block0);

        // Worker 0 exits with error. Worker 1 should also unblock.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), hasher.recv()).await;

        assert!(result.is_ok());

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
