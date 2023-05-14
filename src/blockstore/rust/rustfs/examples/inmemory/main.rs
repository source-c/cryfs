mod device;
mod dir;
mod file;
mod node;
mod symlink;

use device::InMemoryDevice;

const USAGE: &str = "Usage: inmemoryfs [mountdir]";

fn main() {
    // TODO Use clap for argument parsing

    env_logger::init();

    let mut args = std::env::args();
    let _executable = args.next().unwrap();
    let mountdir = args.next().expect(USAGE);
    assert!(args.next().is_none(), "{}", USAGE);

    let device = |uid, gid| InMemoryDevice::new(uid, gid);

    cryfs_rustfs::fuse_mt::mount(device, mountdir).unwrap();
}
