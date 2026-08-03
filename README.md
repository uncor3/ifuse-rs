# ifuse-rs

`ifuse-rs` is a Rust rewrite of
[ifuse](https://github.com/libimobiledevice/ifuse) for Linux and Windows. It
mounts an iOS device's media directory, AFC2 root, application Documents
directory, or complete application container.

Device communication uses the pure-Rust
[`idevice`](https://crates.io/crates/idevice) crate. Linux mounts use
[`fuser`](https://crates.io/crates/fuser), while Windows mounts use WinFsp.

## Requirements

- Linux: FUSE, `usbmuxd`, and a paired/trusted iOS device.
- Windows: WinFsp and a paired/trusted iOS device.
- Appropriate device and mount permissions on the host.

## CLI

The `bin` feature is enabled by default and builds `ifuse-rs`:

```text
ifuse-rs MOUNTPOINT
ifuse-rs -u UDID MOUNTPOINT
ifuse-rs -n -u UDID MOUNTPOINT
ifuse-rs --documents org.example.app MOUNTPOINT
ifuse-rs --container org.example.app MOUNTPOINT
ifuse-rs --root MOUNTPOINT
ifuse-rs --list-apps
```

Linux accepts standard FUSE options through `-o`. Windows rejects non-empty
FUSE-specific options instead of silently ignoring them. SIGINT/SIGTERM on
Linux and the corresponding Windows console shutdown events cleanly unmount
the filesystem.

## Library

```rust,no_run
use ifuse_rs::IfuseBuilder;

# async fn example() -> ifuse_rs::Result<()> {
let mount = IfuseBuilder::usb("device-udid")
    .mount_point("/mnt/iphone")
    .mount()
    .await?;

mount.unmount().await?;
# Ok(())
# }
```

`DeviceTarget` also supports usbmuxd network devices and direct TCP providers
with a pairing file. `MountHandle` is cloneable, and unmounting is idempotent.

Consumers that already select OpenSSL for `idevice` can disable defaults and
enable the matching provider:

OpenSSL may be required because rustls which `idevice` uses by default doesn't support old SSL/TLS protocols.

```toml
ifuse-rs = { version = "0.1.0", default-features = false, features = ["openssl"] }
```

## Platform support and license

Linux and Windows are supported. Other operating systems fail at compile time.
The project is licensed under GPL-3.0-or-later.
