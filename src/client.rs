use crate::hash_file::hash_file;
use crate::sync::Loan;
use crate::HashAlgorithm;
use crate::{check_status, write_string, ResumableReadString, ResumableWriteFileBlock};
use anyhow::anyhow;
use futures::future::OptionFuture;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_stream::StreamExt;

pub async fn new_process(
    address: String,
    secret: String,
    src_path: PathBuf,
    dest: PathBuf,
    force_truncate: bool,
    workers: u8,
    block_size: usize,
    hash_algorithm: HashAlgorithm,
) -> Result<(), anyhow::Error> {
    let mut stream = TcpStream::connect(address).await?;

    // first send secret
    stream.write_u8(secret.len().try_into()?).await?;
    stream.write_all(secret.as_bytes()).await?;
    check_status(&mut stream).await?;

    // send dest path
    write_string(
        &mut stream,
        dest.to_str()
            .ok_or(anyhow!("Invalid characters in destination path"))?,
    )
    .await?;

    // open source file
    let mut src = File::open(&src_path)?;
    let src_size = src.seek(SeekFrom::End(0))?;
    src.rewind()?;

    // send block size, file size, force flag and hash algorithm
    stream.write_u64(block_size.try_into()?).await?;
    stream.write_u64(src_size).await?;
    stream.write_u8(if force_truncate { 1 } else { 0 }).await?;
    stream.write_u8(hash_algorithm.into()).await?;

    check_status(&mut stream).await?;

    // Start the file hashing
    let mut hasher = hash_file(&src_path, block_size, workers, hash_algorithm)?;
    let mut processing_hash = None;
    let mut hash_comparison_over = false;

    // Event loop
    let mut resumable_read_string = ResumableReadString::new();
    let mut block_idx = 0usize;
    let end_block_idx = (src_size as f32 / block_size as f32).ceil() as usize;
    let mut blocks_idx_to_send = VecDeque::new();
    let mut resumable_write_block: Option<ResumableWriteFileBlock> = None;
    let (mut stream_rx, mut stream_tx) = tokio::io::split(stream);

    let mut progress_timer = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(
        std::time::Duration::from_secs(3),
    ));

    loop {
        let write_next_block_future = OptionFuture::from(
            resumable_write_block
                .as_mut()
                .map(|rwb| rwb.write_to(&mut stream_tx)),
        );

        tokio::select! {
            biased;

            // Display progression
            Some(_) = progress_timer.next() => {
                println!("{} / {} ({}%)", block_idx, end_block_idx, block_idx*100/end_block_idx);
            },

            // Next block has been fully written
            Some(_) = write_next_block_future => {
                resumable_write_block = None;

                if hash_comparison_over && blocks_idx_to_send.is_empty() {
                    break;
                } else {
                    prepare_next_write_block(block_size, &mut blocks_idx_to_send, &mut resumable_write_block).await?;
                }
            },

            // Hash of next block from server
            res = resumable_read_string.read_on(&mut stream_rx), if processing_hash.is_none() => match res {
                Ok(hash) => { processing_hash = Some(hash); },
                Err(_) => break, // connection closed by server, stop the event loop
            },

            // Next ordered block hash, compare hash with local file and add block idx to "send list" if different
            Some((_, block_data)) = hasher.recv(), if processing_hash.is_some() && !hash_comparison_over => {
                if block_data.hash != processing_hash.unwrap() {
                    blocks_idx_to_send.push_back((block_idx, block_data.data));
                    prepare_next_write_block(block_size, &mut blocks_idx_to_send, &mut resumable_write_block).await?;
                }
                processing_hash = None;
                block_idx += 1;
                if block_idx == end_block_idx {
                    hash_comparison_over = true;
                    if resumable_write_block.is_none() {
                        break;
                    }
                }
            },

            else => break,
        };
    }

    println!("{} / {} (100%)", end_block_idx, end_block_idx);

    Ok(())
}

async fn prepare_next_write_block(
    block_size: usize,
    blocks_idx_to_send: &mut VecDeque<(usize, Loan<Vec<u8>>)>,
    resumable_write_block: &mut Option<ResumableWriteFileBlock>,
) -> Result<(), anyhow::Error> {
    if resumable_write_block.is_some() {
        return Ok(());
    }

    let block_idx = blocks_idx_to_send.pop_front();

    *resumable_write_block =
        block_idx.map(|(idx, data)| ResumableWriteFileBlock::new(idx * block_size, data));

    Ok(())
}
