# Unreleased

- Fix a TCP backpressure deadlock.

# 1.2.0 (2026-02-17)

- New flag `--port` to specify server port in `remote_start` mode.

- Fix possible deadlock when hasher fails to read file.

- Limit acceptable size for hash and destination path to prevent abuse.

- Update to Rust 2024 edition with MSRV 1.85 and updated dependencies.

# 1.1.1 (2024-10-08)

- Fix last bloc not properly truncated

# 1.1.0 (2024-02-06)

- New flag `--hash` which allow to specify the hash algorithm used for computing blocks checksums.

- Server and client check that they are using the same version of the procotol before going on.

# 1.0.0 (2024-02-01)

Initial release
