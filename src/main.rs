use anyhow::{anyhow, bail};
use clap::{Parser, ValueEnum};
use daemonize::Daemonize;
use int_enum::IntEnum;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;

mod client;
mod common;
mod hash_file;
mod remote_start;
mod server;
mod sync;
mod utils;

use common::{
    check_status, read_string, write_status, write_string, ResumableAsyncWriteAll,
    ResumableReadFileBlock, ResumableReadString, ResumableWriteFileBlock, ResumableWriteString,
    StatusCode, PROTOCOL_VERSION,
};

use remote_start::{remote_start_server, RemoteStartOptions};

#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, IntEnum, ValueEnum)]
pub enum HashAlgorithm {
    #[default]
    CRC32 = 1,
    MD5 = 2,
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, verbatim_doc_comment)]
/// Utility program to synchronise large files to a remote machine.
///
/// This program needs to be run on both machines. The machine that will receive the file content acts as the server while the other machine will connect to it as a client.
/// Both machines will scan through their file to compute checksums of all their blocks. Server will send all these checksums to the client which will compare them to theirs.
/// If a block has a different checksum, the client send this block to the server which can write it to disk.
///
/// This has the benefit to only send blocks which were modified over the network to minimize the bandwidth used.
/// This also allows to split the checksums computation across multiple threads and minimize the time needed to search for diverging blocks.
///
/// For example, on the receiving side, you may start a server using 8 threads to compute checksums and exiting after having completed a sync (with the one-shot flag -o):
///
///     deltasync --server -w 8 -o --secret "some_secure_secret"
///
/// On the client side, you'll then issue a command like this:
///
///     deltasync -w 8 --secret "some_secure_secret" --ip <IP_OF_SERVER> file_to_sync /remote/path/to/file
///
/// You can save the need to start the server by using the --remote-start flag but it will connect to remote machine using SSH first to upload the binary and start the server for you.
///
/// These two commands can be simplified into this one on the client side (secret is not needed anymore as it will generate a random one):
///
///     deltasync -w 8 --ip <IP_OF_SERVER> --remote-start file_to_sync /remote/path/to/file
///
struct Args {
    /// Executes in server mode.
    #[arg(long)]
    server: bool,

    /// IP address of the server (or listening interface if --server is set) [default to 0.0.0.0 for server, and 127.0.0.1 for client]
    #[arg(long)]
    ip: Option<String>,

    /// Listen port of the server
    #[arg(short = 'p', long, default_value_t = 15000)]
    port: u16,

    /// Secret for the connection to be established
    #[arg(long)]
    secret: Option<String>,

    /// Stop the server after first client serviced (one-shot mode)
    #[arg(short = 'o', long)]
    one_shot: bool,

    /// If the destination file already exists with another size, truncate it instead of exiting with an error
    #[arg(short = 'T', long("truncate"))]
    force_truncate: bool,

    /// Number of workers to spawn to compute block hashes
    #[arg(short, long, default_value_t = 1)]
    workers: u8,

    /// Size of blocks (in bytes)
    #[arg(short = 'b', default_value_t = 1048576)]
    block_size: usize,

    /// Hash algorithm to use
    #[arg(long, default_value_t, value_enum)]
    hash: HashAlgorithm,

    /// Ephemeral mode: the executable is removed when the process is started. This is used in remote end when --remote-start flag is set to avoid polluting the temporary directory.
    #[arg(short = 'e', long)]
    ephemeral: bool,

    /// Detach the process in the background
    #[arg(short = 'd')]
    detached: bool,

    /// Starts the server on the remote end automatically (Requires sshd to run on the server)
    #[arg(short = 'r', long)]
    remote_start: bool,

    /// SSH user to connect to the remote end (only used if --remote-start is set) [default: current user name]
    #[arg(short = 'U', long)]
    remote_start_user: Option<String>,

    /// SSH port of the remote end (only used if --remote-start is set)
    #[arg(short = 'P', long, default_value_t = 22)]
    remote_start_port: u16,

    /// Number of workers to spawn to compute block hashes on the remote end (only used if --remote-start is set) [default: same value as --workers]
    #[arg(short = 'W', long)]
    remote_start_workers: Option<u8>,

    /// Identity file to use to connect to SSH (only used if --remote-start is set)
    #[arg(short = 'i', long)]
    identity_file: Option<String>,

    /// Local file to synchronise (mandatory in client mode)
    src: Option<String>,

    /// Defines the destination path on the server [default: same absolute path as <SRC>]
    dest: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();

    if args.ephemeral {
        std::fs::remove_file(std::env::current_exe()?)?;
    }

    if args.detached {
        let daemonize = Daemonize::new();
        daemonize.start()?;
    }

    let secret = args.secret.unwrap_or("".to_string());
    if secret.len() > 255 {
        bail!("The secret cannot exceed 255 characters");
    }

    if args.server {
        // Server mode
        let ip = args.ip.unwrap_or("0.0.0.0".to_string());
        let address = ip + ":" + &args.port.to_string();

        let client_count = Arc::new(AtomicU8::new(0));

        let mut client_stream = TcpListenerStream::new(TcpListener::bind(address).await?);
        while let Some(Ok(mut client)) = client_stream.next().await {
            let count = client_count.clone();

            if count
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let secret = secret.clone();
                let handle = tokio::spawn(async move {
                    let client_addr = client
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or("(unknown)".to_string());
                    println!("Process client {}...", client_addr);

                    match server::process_new_client(&mut client, secret, args.workers).await {
                        Ok(status) => {
                            let _ = write_status(&mut client, status).await;
                        }
                        Err(e) => {
                            eprintln!("Error with client {}: {}", client_addr, e);
                        }
                    }

                    count.store(0, Ordering::Release);

                    println!("Process for client {} is over", client_addr);
                });

                if args.one_shot {
                    let _ = handle.await;
                    break;
                }
            } else {
                let _ = write_status(&mut client, StatusCode::ClientAlreadyConnected).await;
            }
        }
    } else {
        // Client mode

        let ip = args.ip.unwrap_or("127.0.0.1".to_string());

        let secret = if args.remote_start {
            let username = users::get_current_username();
            let username = args
                .remote_start_user
                .as_deref()
                .or_else(|| username.as_ref().and_then(|u| u.to_str()))
                .ok_or(anyhow!("Failed to get current username"))?;

            remote_start_server(RemoteStartOptions {
                address: ip.clone(),
                port: args.remote_start_port,
                username: username.to_string(),
                identity_file: args.identity_file,
                workers: args.remote_start_workers.unwrap_or(args.workers),
            })?
        } else {
            secret
        };

        // canonicalise paths
        let src = utils::absolute_path(args.src.ok_or(anyhow!("Missing source file"))?)?;
        let dest = if let Some(dest) = args.dest {
            utils::absolute_path(dest)?
        } else {
            src.clone()
        };

        let address = ip + ":" + &args.port.to_string();

        if let Err(e) = client::new_process(client::ClientProcessOptions {
            address,
            secret,
            src_path: src,
            dest,
            force_truncate: args.force_truncate,
            workers: args.workers,
            block_size: args.block_size,
            hash_algorithm: args.hash,
        })
        .await
        {
            eprintln!("Error: {}", e);
        }
    }

    Ok(())
}
