use crate::sync::{loan, Loan};
use crate::sync::{ordered_channel, OrderedReceiver};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;

pub struct FileBlockData {
    pub hash: String,
    pub data: Loan<Vec<u8>>,
}

pub struct HashStream {
    rx: OrderedReceiver<(usize, FileBlockData)>,
    join_handles: Vec<std::thread::JoinHandle<()>>,
}

pub fn hash_file(
    path: impl AsRef<Path>,
    block_size: usize,
    workers: u8,
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
    for i in 0..(workers as usize) {
        let mut tx = tx.clone();
        let path = path.clone();
        let handle = thread::spawn(move || {
            let mut offset = i * block_size;
            let mut block_idx = i;
            let mut file = {
                match File::open(&path) {
                    Ok(file) => file,
                    Err(_) => {
                        return;
                    }
                }
            };
            if file.seek(SeekFrom::Start(offset as u64)).is_err() {
                return;
            }

            let mut buffer = vec![0; block_size];

            while offset < file_size {
                use md5::{Digest, Md5};
                use std::io::Read;

                let hash = {
                    let buf = &mut buffer;

                    let current_block_size = {
                        let rem = file_size - offset;
                        if rem < block_size {
                            rem
                        } else {
                            block_size
                        }
                    };
                    let buf = &mut buf[0..current_block_size];

                    if file.read_exact(buf).is_err() {
                        break;
                    }

                    {
                        let hash = Md5::digest(buf);
                        hex::encode(hash)
                    }
                };

                let (loaned, reclaimer) = loan(buffer);
                let mut block_data = FileBlockData::new(loaned);
                block_data.hash = hash;

                if tx.send(block_idx, (block_idx, block_data)).is_err() {
                    break; // receiver closed, indicating we do not want hash any longer
                }

                buffer = reclaimer.blocking_reclaim();

                if file
                    .seek(SeekFrom::Current(
                        (((workers - 1) as usize) * block_size) as i64,
                    ))
                    .is_err()
                {
                    break;
                }
                offset += (workers as usize) * block_size;
                block_idx += workers as usize;
            }
        });
        join_handles.push(handle);
    }

    Ok(HashStream { rx, join_handles })
}

impl HashStream {
    pub async fn recv(&mut self) -> Option<(usize, FileBlockData)> {
        self.rx.recv().await
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

        let mut hasher = hash_file("test/file_to_hash.txt", 10, 2).unwrap();

        let assert_hash = |file_block_data: FileBlockData, expected_hash| {
            assert_eq!(file_block_data.hash, expected_hash);
        };

        {
            let (idx, data) = hasher.recv().await.unwrap();
            assert_eq!(idx, 0);
            assert_hash(data, "87569f2a32a30008939dbce402476f9e");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap();
            assert_eq!(idx, 1);
            assert_hash(data, "4bb90b25dbd367acfb66910764a60669");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap();
            assert_eq!(idx, 2);
            assert_hash(data, "2ae5379d67740e8bb6ce27404873efa5");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap();
            assert_eq!(idx, 3);
            assert_hash(data, "330880debc47896dacf1b11aa46ada4a");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap();
            assert_eq!(idx, 4);
            assert_hash(data, "a80a682a9206c851a4dd9d809a20e4c5");
        }

        {
            let (idx, data) = hasher.recv().await.unwrap();
            assert_eq!(idx, 5);
            assert_hash(data, "590f3733f7c58433d9077904b0f1a6ee");
        }

        assert!(hasher.recv().await.is_none());
    }

    #[tokio::test]
    async fn dropped() {
        use super::*;

        let mut hasher = hash_file("test/file_to_hash.txt", 10, 2).unwrap();

        let _ = hasher.recv().await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        // we do not read all and the senders should not cause deadlock because
        // they must not have sent any Loan to the channel (ordered_queue buffer is guaranteed to be empty)
        // So they will detect that rx is dropped on their next call to tx.send()
    }
}
