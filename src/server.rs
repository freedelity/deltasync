use crate::hash_file::hash_file;
use crate::HashAlgorithm;
use crate::{
    read_string, write_status, ResumableAsyncWriteAll, ResumableReadFileBlock,
    ResumableWriteString, StatusCode,
};
use futures::future::OptionFuture;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpStream;

pub async fn process_new_client(
    client: &mut TcpStream,
    secret: String,
    workers: u8,
) -> Result<StatusCode, anyhow::Error> {
    let secret_len = client.read_u8().await?;
    if secret_len != secret.len() as u8 {
        return Ok(StatusCode::InvalidSecret);
    }

    let mut client_secret = vec![0; secret_len as usize];
    client.read_exact(client_secret.as_mut_slice()).await?;
    if client_secret != secret.as_bytes() {
        return Ok(StatusCode::InvalidSecret);
    }

    write_status(client, StatusCode::Ack).await?;

    // get dest path
    let dest_path = read_string(client).await?;

    // get block and file size
    let block_size = client.read_u64().await? as usize;
    let filesize = client.read_u64().await?;
    let force_truncate = !matches!(client.read_u8().await?, 0);

    // get hash algorithm
    let hash_algorithm = match HashAlgorithm::try_from(client.read_u8().await?) {
        Ok(algo) => algo,
        Err(_) => {
            return Ok(StatusCode::UnknownHashAlgorithm);
        }
    };

    match std::fs::OpenOptions::new().append(true).open(&dest_path) {
        Ok(mut dest) => {
            let actual_size = dest.seek(SeekFrom::End(0))?;
            dest.rewind()?;

            if actual_size == filesize || force_truncate {
                dest.set_len(filesize)?;
                dest
            } else {
                return Ok(StatusCode::FileSizeDiffers);
            }
        }
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => {
                let dest = File::create(&dest_path)?;
                dest.set_len(filesize)?;
                dest
            }
            std::io::ErrorKind::PermissionDenied => {
                return Ok(StatusCode::PermissionDenied);
            }
            _ => {
                return Err(e.into());
            }
        },
    };

    write_status(client, StatusCode::Ack).await?;

    // Start the file hashing
    let mut hasher = hash_file(&dest_path, block_size, workers, hash_algorithm)?;

    // Event loop
    let mut resumable_read_file_block = ResumableReadFileBlock::new(block_size, filesize as usize);
    let mut resumable_write_block_to_disk: Option<ResumableAsyncWriteAll<tokio::fs::File>> = None;
    let mut resumable_write_hash: Option<ResumableWriteString> = None;
    let (mut client_rx, mut client_tx) = tokio::io::split(client);

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(dest_path)
        .await?;

    let mut receive_buffer = Vec::with_capacity(block_size);
    let mut write_buffer = Vec::with_capacity(block_size);

    loop {
        let pending_disk = resumable_write_block_to_disk.is_some();
        let pending_hash_write = resumable_write_hash.is_some();

        let write_next_block_to_file_future = OptionFuture::from(
            resumable_write_block_to_disk
                .as_mut()
                .map(|rwa| rwa.write_all(&write_buffer)),
        );

        let write_hash_future = OptionFuture::from(
            resumable_write_hash
                .as_mut()
                .map(|rwa| rwa.write_to(&mut client_tx)),
        );

        tokio::select! {
            biased;

            // write current block to local filesystem
            Some(_) = write_next_block_to_file_future => {
                resumable_write_block_to_disk = None;
            },


            // read file blocks from client
            res = resumable_read_file_block.read_on(&mut client_rx, &mut receive_buffer), if !pending_disk => match res {
                Ok(offset) => {
                    std::mem::swap(&mut receive_buffer, &mut write_buffer);
                    file.seek(SeekFrom::Start((offset).try_into()?)).await?;
                    resumable_write_block_to_disk = Some(ResumableAsyncWriteAll::new(&mut file));
                },
                Err(_) => {
                    // connection closed by client, consider this as all diverging blocks being received
                    break;
                }
            },

            Some(_) = write_hash_future => {
                resumable_write_hash = None;
            },

            // Next ordered block hash, send it to client
            Some((_, block_data)) = hasher.recv(), if !pending_hash_write => {
                resumable_write_hash = Some(ResumableWriteString::new(&block_data.hash));
            },

            else => break,
        };
    }

    Ok(StatusCode::Ack)
}
