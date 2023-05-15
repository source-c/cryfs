use async_trait::async_trait;
use cryfs_rustfs::{Device, FsError, FsResult, Gid, Mode, Statfs, Uid};
use std::path::Path;

use super::dir::InMemoryDirRef;
use super::file::{InMemoryFileRef, InMemoryOpenFileRef};
use super::node::InMemoryNodeRef;
use super::symlink::InMemorySymlinkRef;

pub struct InMemoryDevice {
    rootdir: InMemoryDirRef,
}

impl InMemoryDevice {
    pub fn new(uid: Uid, gid: Gid) -> Self {
        let mode = Mode::default()
            .add_dir_flag()
            .add_user_read_flag()
            .add_user_write_flag()
            .add_user_exec_flag();
        Self {
            rootdir: InMemoryDirRef::new(mode, uid, gid),
        }
    }
}

#[async_trait]
impl Device for InMemoryDevice {
    type Node = InMemoryNodeRef;
    type Dir = InMemoryDirRef;
    type Symlink = InMemorySymlinkRef;
    type File = InMemoryFileRef;
    type OpenFile = InMemoryOpenFileRef;

    async fn load_node(&self, path: &Path) -> FsResult<Self::Node> {
        self.rootdir.load_node_relative_path(path)
    }

    async fn load_dir(&self, path: &Path) -> FsResult<Self::Dir> {
        let node = self.load_node(path).await?;
        match node {
            Self::Node::Dir(dir) => Ok(dir),
            _ => Err(FsError::NodeIsNotADirectory),
        }
    }

    async fn load_symlink(&self, path: &Path) -> FsResult<Self::Symlink> {
        let node = self.load_node(path).await?;
        match node {
            Self::Node::Symlink(symlink) => Ok(symlink),
            _ => Err(FsError::NodeIsNotASymlink),
        }
    }

    async fn load_file(&self, path: &Path) -> FsResult<Self::File> {
        let node = self.load_node(path).await?;
        match node {
            Self::Node::File(file) => Ok(file),
            Self::Node::Dir(_) => Err(FsError::NodeIsADirectory),
            Self::Node::Symlink(_) => {
                // TODO What's the right error here?
                Err(FsError::UnknownError)
            }
        }
    }

    async fn statfs(&self) -> FsResult<Statfs> {
        todo!()
    }
}
