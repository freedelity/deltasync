# deltasync

deltasync is a tool designed to synchronize large files efficiently by sending only the blocks that have changed, minimizing the bandwidth needed for incremental synchronizations.

## Overview

deltasync works by dividing the file to be synchronized into fixed-length blocks and calculating the checksums for each block.
If the checksums for a block differ, that block is sent and replaces the corresponding block on the receiving end.

For this to work, deltasync needs to run on both machines. On the receiving machine, deltasync functions as a server, awaiting a synchronization request from a client.
On the sending machine, deltasync operates as the client, initiating the synchronization process.

Both the client and the server compute checksums for their respective files. The server sends its checksums to the client, which then compares these to its own.
When a checksum differs, the client transmits the differing block data to the server, which then updates the corresponding block on the disk.

This method significantly reduces bandwidth usage by only sending blocks with differences.

For files with few differing blocks, the process becomes predominantly CPU-bound, with the majority of time spent identifying these differing blocks.
The division into blocks then enables parallel checksum computation across multiple CPUs, optimizing the use of available CPU resources.

Depending on the scenario, this process is designed to maximize either:
- network or disk usage (whichever is the bottleneck) when the process is IO-bound, as is common during initial synchronizations where files often differ significantly
- checksum processing speed when the process becomes CPU-bound

## Usage

deltasync must run on the receiving side as a server:

```sh
deltasync --server -w 8 --secret "secure_secret"
```

This will accept sync requests from clients. This will only accept one client at a time but will keep running after having completed a request. This can be useful for a machine that repeatedly needs to receive file updates. The flag `-d` allows to run the server in a detached mode (daemon) if useful. Make sure to use a random secret to avoid letting any client write files on your machine.

On the sending side, you can now initiate file synchronizations by running deltasync as a client (note that the remote path must be absolute):

```sh
deltasync -w 8 --secret "secure_secret" --ip <SERVER_IP> path/to/local/file /path/to/remote/file
```

You can also use the `--remote-start` flag in order to avoid having to start the server on the remote end. The client will connect to the remote machine in SSH, upload the binary and execute it as a server.
So you can simply issue this single command:

```sh
deltasync -w 8 --remote-start --ip <SERVER_IP> path/to/local/file /path/to/remote/file
```

Use the `--help` flag to get a full list of the available options.


## Installation

Right now, you can only install this by building it from source.
A prebuilt binary will be available from the releases page soon.

### Build from source

You can simply build this with the **Rust** toolchain (using **cargo** package manager):

```sh
cargo build --release
```

## License

This project is licensed under the [MIT license].

[MIT license]: https://github.com/freedelity/deltasync/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in deltasync by you, shall be licensed as MIT, without any additional
terms or conditions.

## Credits

This is inspired from [blocksync](https://github.com/theraser/blocksync), thanks to its creator and maintainers for this great tool.
