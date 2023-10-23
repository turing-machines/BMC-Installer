use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use std::thread;
use std::time::Duration;
use std::{fs, path::Path};

pub const BANNER: &str = r"
 _____ _   _ ____  ___ _   _  ____ 
|_   _| | | |  _ \|_ _| \ | |/ ___|
  | | | | | | |_) || ||  \| | |  _ 
  | | | |_| |  _ < | || |\  | |_| |
  |_|  \___/|_| \_\___|_| \_|\____|
";

/// Set up the basic environment (e.g. mount points).
pub fn setup_initramfs() -> anyhow::Result<()> {
    // Handle mounts
    for (mount_dev, mount_path, mount_type) in [
        (None, "/dev", "devtmpfs"),
        (None, "/proc", "proc"),
        (None, "/sys", "sysfs"),
    ] {
        let path = Path::new(mount_path);

        if !path.is_dir() {
            fs::create_dir(path)?;
        }

        let result = mount(
            mount_dev.or(Some(path)),
            path,
            Some(mount_type),
            MsFlags::empty(),
            None::<&str>,
        );

        match result {
            // Ignore EBUSY, which indicates that the mountpoint is already mounted.
            Err(errno) if errno == Errno::EBUSY => (),
            r => r?,
        };
    }

    Ok(())
}

/// Sleep until the user cuts power.
pub fn wait_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
