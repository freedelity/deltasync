# deltasync - Architecture Guide

deltasync is a bandwidth-efficient file synchronization tool that transfers only the changed blocks of large files between machines. It divides files into fixed-size blocks (default 1 MB), computes checksums for each block on both sides, and only transmits blocks whose checksums differ.

Inspired by [blocksync](https://github.com/theraser/blocksync), reimplemented in Rust with async I/O.

## Project Structure

```
src/
  main.rs          CLI parsing, server accept loop, client setup
  client.rs        Client-side sync logic
  server.rs        Server-side sync logic
  common.rs        Wire protocol types and resumable I/O helpers
  hash_file.rs     Parallel file hashing with worker threads
  sync.rs          Concurrency primitives (Loan, OrderedChannel)
  remote_start.rs  SSH-based remote server startup
  utils.rs         Path utilities
test/
  file_to_hash.txt Test fixture for hash_file unit tests
.github/workflows/
  ci.yml           Build + test + lint (multi-arch, MSRV 1.70)
  release.yml      Tag-triggered release (9 architectures)
```

## How It Works (High-Level)

```
CLIENT                                SERVER
  |                                     |
  |--- secret, protocol version ------->|  handshake
  |--- dest path, block_size, etc. ---->|
  |<------------ status (Ack) ---------|
  |                                     |
  |  [spawn N hash workers]            |  [spawn N hash workers]
  |                                     |
  |<---------- hash(block 0) ----------|  server sends hashes
  |  compare with local hash(block 0)  |
  |  if different:                      |
  |----------- block 0 data ---------->|  client sends changed blocks
  |                                     |  server writes block to disk
  |<---------- hash(block 1) ----------|
  |  ...repeats until all blocks done  |
  |                                     |
  |  [close connection]                |
```

The diagram above is simplified to show the logical flow. In practice, hash comparisons and block transfers happen concurrently: both sides compute hashes in parallel using worker threads, the server streams its hashes to the client, and the client pipelines diverging block data back without waiting for acknowledgement.

## Data Flow Overview

This section traces how block data flows through the system end-to-end on the client side, tying together the concurrency primitives, hashing, and network I/O:

1. `hash_file()` spawns N worker threads. Each worker reads a block from disk into its own `Vec<u8>` buffer, computes its hash, then calls `loan(buffer)` to get a `(Loan<Vec<u8>>, Reclaim<Vec<u8>>)` pair.
2. The worker wraps the hash string and the `Loan` into a `FileBlockData` and sends it through the `OrderedChannel` as `(block_idx, FileBlockData)`. The worker then blocks on `reclaimer.blocking_reclaim()`, waiting for the buffer to be returned.
3. The client's event loop receives `FileBlockData` from the `HashStream` (which wraps the `OrderedChannel` receiver). It compares the hash with the server's hash for the same block.
4. If the hashes differ, the client pushes `(block_idx, Loan<Vec<u8>>)` into a send queue. The loaned buffer contains the actual block bytes read by the hash worker -- no re-read from disk is needed.
5. `prepare_next_write_block()` pops from the queue and creates a `ResumableWriteFileBlock` that writes the file offset and the block data to the network.
6. Once the `ResumableWriteFileBlock` finishes and is dropped, the `Loan` is dropped with it, returning the buffer to the hash worker via the oneshot channel. The worker can then reuse the buffer for its next block.

On the server side, `FileBlockData` is only used for its hash string (sent to the client via `ResumableWriteString`). The loaned buffer is dropped immediately after extracting the hash, returning it to the worker.

## Module Reference

### `main.rs` - Entry Point

**What it does:** Parses CLI arguments, sets up the Tokio runtime, and dispatches to either server or client mode.

**Key types:**
- `Args` - clap-derived struct with all CLI options
- `HashAlgorithm` - enum: `CRC32` (default) or `MD5`

**Execution flow:**
1. Parse CLI args via `clap`
2. If `--ephemeral`: delete own binary (used by remote-start to clean up)
3. If `-d`: daemonize the process
4. If `--server`: bind TCP, accept connections in a loop (one client at a time, enforced by `AtomicU8` counter)
5. Else (client mode): optionally run remote-start via SSH, then call `client::new_process()`

**Second client rejection:** When a client is already being served, the `AtomicU8` counter prevents a second client from being processed. The server immediately sends a `ClientAlreadyConnected` status byte on the raw TCP connection (before any handshake) and moves on. The second client receives this as an error from `check_status()`.

---

### `client.rs` - Client Logic

**What it does:** Connects to the server, performs the handshake, hashes the local file, receives server hashes, compares them, and sends any differing blocks.

**Key types:**
- `ClientProcessOptions` - all parameters needed for a sync session
- `prepare_next_write_block()` - pops the next diverging block from the queue

**Sync event loop:** Uses `tokio::select!` with biased polling (branches are checked in listed order, giving priority to earlier branches). The ordering is intentional -- sending queued blocks takes priority over receiving new hashes, which ensures the send queue is drained before more diverging blocks are discovered and queued:
1. **Progress timer** - prints `X / Y (Z%)` every 3 seconds
2. **Write block** - sends the next queued diverging block to the server via `ResumableWriteFileBlock`
3. **Read server hash** - receives the next hash from the server via `ResumableReadString`
4. **Compare hashes** - receives local hash from `HashStream`, compares with server hash, queues block if different

The loop exits when all hashes have been compared and all diverging blocks have been sent. The client then closes the TCP connection, which the server interprets as sync completion.

---

### `server.rs` - Server Logic

**What it does:** Receives a client connection, validates credentials, opens/creates the destination file, hashes it, streams hashes to the client, and writes incoming blocks to disk.

**Function:** `process_new_client()`

**Handshake:**
1. Read and validate secret
2. Check protocol version
3. Read destination path, block size, file size, force truncate flag, hash algorithm
4. Open or create destination file (see file size handling below)
5. Send `Ack`

**File size handling during handshake:**
- If the destination file exists and its size matches the source file size: proceed normally.
- If the destination file exists but sizes differ and `--truncate` is set: the file is resized (truncated or extended) to match the source size.
- If the destination file exists but sizes differ and `--truncate` is NOT set: the server returns `FileSizeDiffers` and the sync is rejected.
- If the destination file does not exist: it is created and set to the source file size.
- If the destination path is not accessible: the server returns `PermissionDenied`.

**Sync event loop:** Uses `tokio::select!` with biased polling (branches are checked in listed order, giving priority to earlier branches). The ordering is intentional -- completing disk writes takes priority over receiving new blocks from the client, which prevents unbounded accumulation of received-but-not-yet-written data:
1. **Write block to disk** - writes the previously received block via `ResumableAsyncWriteAll`
2. **Read block from client** - receives block data via `ResumableReadFileBlock` (guarded: disabled while a disk write is pending)
3. **Write hash to client** - sends next hash via `ResumableWriteString`
4. **Get next hash** - receives next computed hash from `HashStream`

**Double-buffering:** The server maintains two pre-allocated buffers: `receive_buffer` and `write_buffer`. When a block is fully received into `receive_buffer`, the two buffers are swapped (`std::mem::swap`, an O(1) pointer swap -- no data is copied). The write_buffer is then written to disk asynchronously while `receive_buffer` is immediately ready for the next block. Reading from the client is disabled while a disk write is in progress (guarded by `!pending_disk`).

The server detects sync completion when the client closes the TCP connection.

---

### `common.rs` - Wire Protocol

**What it does:** Defines the binary protocol, status codes, and resumable I/O structs that allow partial reads/writes to be safely used inside `tokio::select!` branches.

**Protocol constants:**
- `PROTOCOL_VERSION = 1`

**Status codes** (`StatusCode` enum):

| Code | Value | Meaning |
|------|-------|---------|
| `Ack` | 0 | Success |
| `InvalidSecret` | 1 | Authentication failure |
| `FileSizeDiffers` | 2 | File size mismatch (and no `--truncate`) |
| `PermissionDenied` | 3 | Cannot access destination file |
| `ClientAlreadyConnected` | 4 | Server is busy with another client |
| `UnknownHashAlgorithm` | 5 | Hash algorithm not supported |
| `UnsupportedProtocolVersion` | 6 | Protocol version mismatch |

**Simple protocol helpers:**
- `write_string()` / `read_string()` - length-prefixed string (u64 length + UTF-8 bytes)
- `write_status()` / `check_status()` - single-byte status code

**Resumable I/O structs** (the core pattern for cancel-safe `select!` usage):
- `ResumableReadString` - reads a length-prefixed string across multiple `select!` iterations
- `ResumableWriteString` - writes a length-prefixed string across multiple iterations
- `ResumableWriteFileBlock` - writes `u64 offset + block data` to network
- `ResumableReadFileBlock` - reads `u64 offset + block data` from network, computes the actual block size from the offset and known file size (handles the last block being smaller than `block_size`)
- `ResumableAsyncWriteAll` - writes a buffer to an `AsyncWrite` across multiple iterations

These structs exist because `tokio::select!` can cancel a future mid-execution. Each struct tracks its byte offset internally, so it can resume from exactly where it was interrupted on the next `select!` iteration. For example, if a `ResumableWriteFileBlock` has written 3 of 8 offset bytes when cancelled, it will write the remaining 5 bytes on the next poll before continuing with the block data.

---

### `sync.rs` - Concurrency Primitives

**What it does:** Provides two custom synchronization abstractions used by the hashing pipeline.

#### Loan Pattern

Allows a buffer to be temporarily transferred to another thread and reclaimed after use, enabling zero-copy buffer reuse between hash worker threads and the network I/O task.

```
hash worker                       network task
    |                                  |
    |-- loan(buffer) ----------------->|  buffer is "loaned"
    |                                  |  network task uses buffer
    |<-- buffer returned (on drop) ----|  Loan<T> drops, buffer flows back
    |  reclaimer.blocking_reclaim()    |
    |  (worker reuses same buffer)     |
```

- `loan(value)` returns `(Loan<T>, Reclaim<T>)`
- `Loan<T>` implements `Deref`/`DerefMut` for transparent access (the holder can read/write the inner value as if it owned it)
- When `Loan<T>` is dropped, the value is sent back through a tokio `oneshot` channel
- `Reclaim::blocking_reclaim()` blocks the calling thread until the loan is returned

#### Ordered Channel

A channel where senders can send values out of order but the receiver always reads them in sequential order (0, 1, 2, ...).

```
Worker 0: send(idx=2, "C") → blocks until receiver wants idx=2
Worker 1: send(idx=0, "A") → sends immediately (receiver wants idx=0)
Worker 2: send(idx=1, "B") → blocks until receiver wants idx=1

Receiver: recv() → "A", recv() → "B", recv() → "C"
```

Key properties:
- Senders block until the receiver is requesting their specific index
- Buffer size is 1 (bounded), so memory usage is bounded to N workers
- `recv()` is cancel-safe (critical for `tokio::select!` usage)
- When the receiver is dropped, all senders are unblocked and get an error

---

### `hash_file.rs` - Parallel File Hashing

**What it does:** Spawns N OS threads that hash a file's blocks in parallel using a strided access pattern (each worker processes every Nth block), delivering results in order through an `OrderedChannel`.

**Key types:**
- `FileBlockData` - a block's hash string + its loaned data buffer (`Loan<Vec<u8>>`)
- `HashStream` - wraps the `OrderedReceiver` + worker `JoinHandle`s. Both `client.rs` and `server.rs` call `hash_file()` to obtain a `HashStream`

**Function:** `hash_file(path, block_size, workers, algorithm)`

**How workers are distributed (strided pattern):**
```
With 3 workers and 9 blocks:
  Worker 0: blocks 0, 3, 6
  Worker 1: blocks 1, 4, 7
  Worker 2: blocks 2, 5, 8
```

Each worker:
1. Opens the file independently (each worker needs its own file descriptor to seek and read without synchronization across threads)
2. Seeks to its starting offset (`worker_id * block_size`)
3. Reads a block into its buffer (resized to `min(block_size, remaining_bytes)` for the last block)
4. Computes the block's hash
5. Creates a `Loan` of the buffer and sends `(block_idx, FileBlockData)` through the ordered channel
6. Waits for the buffer to be reclaimed (zero-copy reuse)
7. Seeks forward by `(workers - 1) * block_size`, repeats until EOF

**Supported hash algorithms** (`hash_data()`):
- `CRC32` - uses `crc32c` crate (hardware-accelerated SSE 4.2)
- `MD5` - uses `md-5` crate

---

### `remote_start.rs` - SSH Remote Server Setup

**What it does:** Connects to the remote machine via SSH, uploads the current binary, and starts it as a server with a randomly generated secret.

**Function:** `remote_start_server(options)`

**SSH authentication order:**
1. Identity file (`-i` flag), if provided
2. SSH agent
3. Password prompt (via `rpassword`)

**Remote setup sequence:**
1. `mktemp --tmpdir deltasync.XXXXXXXX` - create temp file
2. `cat > <tmpfile>` - upload binary
3. `chmod +x <tmpfile>` - make executable
4. `<tmpfile> --server -d -e -o --port <port> --secret <random> -w <workers>` - start server (detached, ephemeral, one-shot)

The ephemeral flag (`-e`) causes the remote binary to self-delete on startup. The one-shot flag (`-o`) makes it exit after one sync.

---

### `utils.rs` - Path Utilities

**Function:** `absolute_path(path)` - converts a relative path to absolute using `path-clean` for normalization.

---

## Wire Protocol Specification

All multi-byte integers are encoded in **big-endian** byte order.

### Handshake (client to server)

```
Client → Server:  u8 (secret length) + bytes[length]         (secret)
Server → Client:  u8                                          (status)
Client → Server:  u8                                          (protocol version)
Server → Client:  u8                                          (status)
Client → Server:  u64 (length) + UTF-8 bytes                 (destination path)
Client → Server:  u64                                         (block size)
Client → Server:  u64                                         (file size)
Client → Server:  u8                                          (force truncate: 0 or 1)
Client → Server:  u8                                          (hash algorithm: 1=CRC32, 2=MD5)
Server → Client:  u8                                          (status)
```

### Sync phase (bidirectional)

```
Server → Client (continuous):
  u64 (length) + UTF-8 hex string                            (block hash)

Client → Server (for each diverging block):
  u64 (file offset) + bytes[actual_block_size]               (block data)
```

The block data length is not sent explicitly. The receiver computes it as `min(block_size, file_size - offset)`, which handles the last block being smaller than `block_size`.

The client closes the TCP connection when all diverging blocks have been sent. The server interprets the connection close as sync completion. If the connection is interrupted mid-transfer, the server treats it the same way -- blocks written to disk before the interruption are persisted, but the sync is incomplete.

## Key Design Decisions

### Single-threaded async runtime

The Tokio runtime uses `flavor = "current_thread"`. All async I/O (networking, disk writes on the server) runs on a single thread. CPU-intensive hashing runs on separate OS threads spawned by `hash_file`. This avoids the overhead of a multi-threaded runtime while still parallelizing the CPU-bound work.

### Resumable I/O structs

Standard async read/write helpers (like `write_all`) are not cancel-safe: if a `tokio::select!` branch cancels them mid-write, progress is lost. The `Resumable*` structs in `common.rs` solve this by tracking byte offsets internally, so they can be safely used across multiple `select!` iterations.

### Ordered channel with blocking senders

The ordered channel guarantees that:
- Results arrive at the receiver in order even though workers process blocks in parallel
- Memory is bounded: senders block until the receiver consumes their value
- No accumulation of out-of-order results in memory

### Loan pattern for buffer reuse

Rather than allocating a new `Vec<u8>` for each block, workers loan their buffer through the channel and reclaim it after the consumer is done. This eliminates per-block allocation overhead.

### Pipelined block transfer

The client doesn't wait for the server to acknowledge each block. It queues diverging blocks and writes them to the network as fast as possible, overlapping with hash comparisons. The server double-buffers received blocks: it swaps between a receive buffer and a write buffer so that the next block can be received as soon as the previous one starts writing to disk.

## Build & Test

```sh
# Build
cargo build --release

# Build with statically linked OpenSSL (portable binary)
cargo build --release --features vendored-openssl

# Run tests
cargo test

# Lint
cargo fmt --check
cargo clippy --all
```

**MSRV (Minimum Supported Rust Version):** Rust 1.70.0

**Release profile** (`Cargo.toml`): LTO (Link-Time Optimization) enabled, single codegen unit, abort on panic, symbols stripped. Produces a small, optimized binary.

**CI** (`.github/workflows/ci.yml`): Tests on stable/beta/nightly/1.70.0, cross-compiles to 9 architectures, runs `rustfmt` and `clippy`.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (single-threaded), networking, file I/O |
| `tokio-stream` | Stream wrappers for TCP listener and interval timer |
| `clap` | CLI argument parsing (derive API) |
| `anyhow` | Error handling (`Result<T, anyhow::Error>` everywhere) |
| `crc32c` | CRC32C checksums (hardware-accelerated) |
| `md-5` | MD5 hashing |
| `hex` | Hex encoding of hash digests |
| `ssh2` | SSH client for remote-start |
| `rpassword` | Password prompt for SSH |
| `int-enum` | Integer-backed enum derive |
| `rand` | Random secret generation |
| `daemonize` | Background daemon mode |
| `path-clean` | Path normalization |
| `users` | Current username lookup |
| `futures` / `futures-core` | `OptionFuture` for conditional `select!` branches (allows branches to be enabled/disabled at runtime) |
| `num` | Numeric traits for ordered channel generic index |
