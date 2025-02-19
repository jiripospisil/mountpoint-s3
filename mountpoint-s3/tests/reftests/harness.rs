use crate::common::{make_test_filesystem, DirectoryReply, ReadReply};
use crate::reftests::generators::{flatten_tree, gen_tree, valid_name_strategy, FileContent, FileSize, Name, TreeNode};
use crate::reftests::reference::{build_reference, File, Node, Reference};
use fuser::FileType;
use futures::executor::ThreadPool;
use futures::future::{BoxFuture, FutureExt};
use mountpoint_s3::{
    fs::{InodeNo, FUSE_ROOT_INODE},
    prefix::Prefix,
    {S3Filesystem, S3FilesystemConfig},
};
use mountpoint_s3_client::mock_client::{MockClient, MockObject};
use proptest::prelude::*;
use proptest_derive::Arbitrary;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;
use std::path::{Component, Path};
use std::sync::Arc;
use tracing::debug;

/// Operations that the mutating proptests can perform on the file system.
// TODO: mkdir, unlink
// TODO: "reboot" (forget all the local inodes and re-bootstrap)
// TODO: incremental writes (test partially written files)
#[derive(Debug, Arbitrary)]
pub enum Op {
    WriteFile(
        #[proptest(strategy = "valid_name_strategy()")] String,
        DirectoryIndex,
        FileContent,
    ),
}

/// An index into the reference model's list of directories. We use this to randomly select an
/// existing directory to operate on (for put, rmdir, etc).
#[derive(Debug, Arbitrary)]
pub struct DirectoryIndex(usize);

impl DirectoryIndex {
    /// Get the full path to the directory at the given index in the reference (wrapping around if
    /// the index is larger than the number of directories)
    fn get<'a>(&self, reference: &'a Reference) -> impl AsRef<Path> + 'a {
        let directories = reference.directories();
        assert!(!directories.is_empty(), "directories can never be empty");
        let idx = self.0 % directories.len();
        &directories[idx]
    }
}

#[derive(Debug)]
pub struct Harness {
    readdir_limit: usize, // max number of entries that a readdir will return; 0 means no limit
    reference: Reference,
    fs: S3Filesystem<Arc<MockClient>, ThreadPool>,
}

impl Harness {
    /// Create a new test harness
    pub fn new(fs: S3Filesystem<Arc<MockClient>, ThreadPool>, reference: Reference, readdir_limit: usize) -> Self {
        Self {
            readdir_limit,
            reference,
            fs,
        }
    }

    /// Run a sequence of mutation operations on the test harness, checking equivalence between the
    /// reference model and file system after each operation.
    pub async fn run(&mut self, ops: Vec<Op>) {
        for op in ops {
            debug!(?op, "executing operation");
            match &op {
                Op::WriteFile(name, directory_index, contents) => {
                    let dir = directory_index.get(&self.reference);
                    let full_path = dir.as_ref().join(name);

                    // Find the inode for the directory by walking the file system tree
                    let mut components = dir.as_ref().components();
                    assert_eq!(components.next(), Some(Component::RootDir));
                    let mut inode = FUSE_ROOT_INODE;
                    for component in components {
                        if let Component::Normal(folder) = component {
                            inode = self
                                .fs
                                .lookup(inode, folder)
                                .await
                                .expect("directory must already exist")
                                .attr
                                .ino;
                        } else {
                            panic!("unexpected path component {component:?}");
                        }
                    }
                    drop(dir);

                    // Random paths can shadow existing ones, so we check that we aren't allowed to
                    // overwrite an existing inode. The existing node could be either a file or
                    // directory; we should fail the same way in both cases.
                    // TODO we have to get pretty lucky to hit this path right now -- try to bias the
                    // search in this direction a bit.
                    let reference_lookup = self.reference.lookup(&full_path);
                    if reference_lookup.is_some() {
                        let mknod = self.fs.mknod(inode, name.as_ref(), libc::S_IFREG, 0, 0).await;
                        assert!(
                            matches!(mknod, Err(libc::EEXIST)),
                            "can't overwrite existing file/directory"
                        );
                    } else {
                        let mknod = self.fs.mknod(inode, name.as_ref(), libc::S_IFREG, 0, 0).await.unwrap();
                        let open = self.fs.open(mknod.attr.ino, libc::O_WRONLY).await.unwrap();

                        // TODO try testing more than one `write` call
                        let bytes = contents.to_boxed_slice();
                        let write = self
                            .fs
                            .write(mknod.attr.ino, open.fh, 0, &bytes, 0, 0, None)
                            .await
                            .unwrap();
                        assert_eq!(write as usize, bytes.len());

                        self.fs.release(mknod.attr.ino, open.fh, 0, None, false).await.unwrap();

                        self.reference.add_file(&full_path, contents);
                    }
                }
            }

            debug!(?op, "checking contents");
            self.compare_contents().await;
        }
    }

    /// Walk the filesystem tree and check that at each level, contents match the reference
    pub async fn compare_contents(&self) {
        let root = self.reference.root();
        self.compare_contents_recursive(FUSE_ROOT_INODE, FUSE_ROOT_INODE, root)
            .await;
    }

    /// Walk a single path through the filesystem tree and ensure each node matches the reference.
    /// Compared to [compare_contents], this avoids doing a `readdir` before `lookup`, and so tests
    /// a different path through the inode code.
    pub async fn compare_single_path(&self, idx: usize) {
        let inodes = self.reference.list_recursive();
        if inodes.is_empty() {
            return;
        }
        let (path, node) = &inodes[idx % inodes.len()];

        let mut parent = FUSE_ROOT_INODE;
        let mut seen_inos = HashSet::from([FUSE_ROOT_INODE]);
        for name in path.iter().take(path.len().saturating_sub(1)) {
            let lookup = self.fs.lookup(parent, name.as_ref()).await.unwrap();
            assert_eq!(lookup.attr.kind, FileType::Directory);
            assert!(seen_inos.insert(lookup.attr.ino));
            parent = lookup.attr.ino;
        }

        let lookup = self.fs.lookup(parent, path.last().unwrap().as_ref()).await.unwrap();
        assert!(seen_inos.insert(lookup.attr.ino));
        match node {
            Node::Directory(_) => {
                assert_eq!(lookup.attr.kind, FileType::Directory);
            }
            Node::File(content) => {
                assert_eq!(lookup.attr.kind, FileType::RegularFile);
                match content {
                    File::Local(_) => unimplemented!(),
                    File::Remote(object) => self.compare_file(lookup.attr.ino, object).await,
                }
            }
        }
    }

    fn compare_contents_recursive<'a>(
        &'a self,
        fs_parent: InodeNo,
        fs_dir: InodeNo,
        ref_dir: &'a Node,
    ) -> BoxFuture<'a, ()> {
        async move {
            let dir_handle = self.fs.opendir(fs_dir, 0).await.unwrap().fh;
            let children = ref_dir.children();
            let mut keys = children.keys().cloned().collect::<HashSet<_>>();

            let mut reply = DirectoryReply::new(self.readdir_limit);
            let _reply = self.fs.readdir(fs_dir, dir_handle, 0, &mut reply).await.unwrap();

            // TODO `stat` on these needs to work
            let e0 = reply.entries.pop_front().unwrap();
            assert_eq!(e0.name, ".");
            assert_eq!(e0.ino, fs_dir);
            let mut offset = e0.offset;

            if reply.entries.is_empty() {
                reply.clear();
                let _reply = self.fs.readdir(fs_dir, dir_handle, offset, &mut reply).await.unwrap();
            }

            let e1 = reply.entries.pop_front().unwrap();
            assert_eq!(e1.name, "..");
            assert_eq!(e1.ino, fs_parent);
            offset = offset.max(e1.offset);

            if reply.entries.is_empty() {
                reply.clear();
                let _reply = self.fs.readdir(fs_dir, dir_handle, offset, &mut reply).await;
                _reply.unwrap();
            }

            while !reply.entries.is_empty() {
                while let Some(reply) = reply.entries.pop_front() {
                    offset = offset.max(reply.offset);

                    let name = &reply.name.as_os_str().to_str().unwrap().to_string();
                    let fs_kind = reply.attr.kind;

                    let lkup = self.fs.lookup(fs_dir, &reply.name).await.unwrap();
                    let attr = lkup.attr;

                    match children.get(name) {
                        Some(node) => {
                            let ref_kind = node.file_type();
                            assert_eq!(
                                fs_kind, ref_kind,
                                "for file {name:?} expecting {ref_kind:?} found {fs_kind:?}"
                            );
                            assert_eq!(
                                attr.ino, reply.ino,
                                "for file {:?} readdir ino {:?} lookup ino {:?}",
                                name, reply.ino, attr.ino
                            );
                            if let Node::File(ref_object) = node {
                                assert_eq!(attr.kind, FileType::RegularFile);
                                match ref_object {
                                    File::Local(_) => todo!("local files are not yet tested"),
                                    File::Remote(object) => self.compare_file(reply.ino, object).await,
                                }
                            } else {
                                assert_eq!(attr.kind, FileType::Directory);
                                // Recurse into directory
                                self.compare_contents_recursive(fs_dir, reply.ino, node).await;
                            }
                            assert!(keys.remove(name));
                        }
                        None => panic!("file {name:?} not found in the reference"),
                    }
                }
                reply.clear();
                let _reply = self.fs.readdir(fs_dir, dir_handle, offset, &mut reply).await.unwrap();
            }

            assert!(
                keys.is_empty(),
                "reference contained elements not in the filesystem: {keys:?}"
            );

            // Not implemented
            // self.fs.releasedir(dir_handle).unwrap();
        }
        .boxed()
    }

    async fn compare_file<'a>(&'a self, fs_file: InodeNo, ref_file: &'a MockObject) {
        let fh = self.fs.open(fs_file, 0x8000).await.unwrap().fh;
        let mut offset = 0;
        const MAX_READ_SIZE: usize = 4_096;
        let file_size = ref_file.len();
        while offset < file_size {
            let mut read = Err(0);
            let num_bytes = MAX_READ_SIZE.min(file_size - offset);
            self.fs
                .read(
                    fs_file,
                    fh,
                    offset as i64,
                    num_bytes as u32,
                    0,
                    None,
                    ReadReply(&mut read),
                )
                .await;
            let fs_bytes = read.unwrap();
            assert_eq!(fs_bytes.len(), num_bytes);
            let ref_bytes = ref_file.read(offset as u64, num_bytes);
            assert_eq!(ref_bytes, fs_bytes);
            offset += num_bytes;
        }
    }
}

/// Read-only reftests that generate random S3 buckets and check the mapping from S3 keys to file
/// paths is correct.
mod read_only {
    use super::*;

    #[derive(Debug)]
    enum CheckType {
        /// Traverse the entire tree recursively with `readdir` and compare the results of every node
        FullTree,
        /// Do a lookup along a single path and compare the leaf node
        SinglePath {
            /// Index into the list of all nodes in the file system
            path_index: usize,
        },
    }

    fn run_test(tree: TreeNode, check: CheckType, readdir_limit: usize) {
        let test_prefix = Prefix::new("test_prefix/").expect("valid prefix");
        let config = S3FilesystemConfig {
            readdir_size: 5,
            ..Default::default()
        };
        let (client, fs) = make_test_filesystem("harness", &test_prefix, config);

        let namespace = flatten_tree(tree);
        for (key, object) in namespace.iter() {
            client.add_object(&format!("{test_prefix}{key}"), object.to_mock_object());
        }

        let reference = build_reference(namespace);

        let harness = Harness::new(fs, reference, readdir_limit);

        futures::executor::block_on(async move {
            match check {
                CheckType::FullTree => harness.compare_contents().await,
                CheckType::SinglePath { path_index } => harness.compare_single_path(path_index).await,
            }
        });
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            .. ProptestConfig::default()
        })]

        #[test]
        fn reftest_random_tree_full(readdir_limit in 0..10usize, tree in gen_tree(5, 100, 5, 20)) {
            run_test(tree, CheckType::FullTree, readdir_limit);
        }

        #[test]
        fn reftest_random_tree_single(tree in gen_tree(5, 100, 5, 20), path_index: usize) {
            run_test(tree, CheckType::SinglePath { path_index }, 0);
        }
    }

    #[test]
    fn random_tree_regression_basic() {
        run_test(
            TreeNode::Directory(BTreeMap::from([(
                Name("-".to_string()),
                TreeNode::Directory(BTreeMap::from([(
                    Name("-".to_string()),
                    TreeNode::File(FileContent(0, FileSize::Small(0))),
                )])),
            )])),
            CheckType::FullTree,
            0,
        );
    }

    #[test]
    fn random_tree_regression_directory_order() {
        run_test(
            TreeNode::Directory(BTreeMap::from([
                (
                    Name("-a-".to_string()),
                    TreeNode::File(FileContent(0, FileSize::Small(0))),
                ),
                (
                    Name("-a".to_string()),
                    TreeNode::Directory(BTreeMap::from([(
                        Name("-".to_string()),
                        TreeNode::File(FileContent(0, FileSize::Small(0))),
                    )])),
                ),
            ])),
            CheckType::FullTree,
            0,
        );
    }

    #[test]
    fn random_tree_regression_invalid_name1() {
        run_test(
            TreeNode::Directory(BTreeMap::from([(
                Name("-".to_string()),
                TreeNode::Directory(BTreeMap::from([(
                    Name(".".to_string()),
                    TreeNode::File(FileContent(0, FileSize::Small(0))),
                )])),
            )])),
            CheckType::FullTree,
            0,
        );
    }

    #[test]
    fn random_tree_regression_invalid_name2() {
        run_test(
            TreeNode::Directory(BTreeMap::from([(
                Name("-".to_string()),
                TreeNode::Directory(BTreeMap::from([(
                    Name("a/".to_string()),
                    TreeNode::File(FileContent(0, FileSize::Small(0))),
                )])),
            )])),
            CheckType::FullTree,
            0,
        )
    }

    #[test]
    fn random_tree_regression_directory_shadow() {
        run_test(
            TreeNode::Directory(BTreeMap::from([(
                Name("a".to_string()),
                TreeNode::Directory(BTreeMap::from([
                    (
                        Name("a/".to_string()),
                        TreeNode::File(FileContent(0, FileSize::Small(0))),
                    ),
                    (
                        Name("a".to_string()),
                        TreeNode::File(FileContent(0, FileSize::Small(0))),
                    ),
                ])),
            )])),
            CheckType::FullTree,
            0,
        )
    }

    #[test]
    fn random_tree_regression_directory_shadow_lookup() {
        run_test(
            TreeNode::Directory(BTreeMap::from([(
                Name("a".to_string()),
                TreeNode::Directory(BTreeMap::from([
                    (
                        Name("a/".to_string()),
                        TreeNode::File(FileContent(0, FileSize::Small(0))),
                    ),
                    (
                        Name("a".to_string()),
                        TreeNode::File(FileContent(0, FileSize::Small(0))),
                    ),
                ])),
            )])),
            CheckType::SinglePath { path_index: 1 },
            0,
        )
    }
}

/// Mutation tests that run a sequence of mutations against a file system and check equivalence to
/// the reference model.
mod mutations {
    use super::*;
    use proptest::collection::vec;

    fn run_test(initial_tree: TreeNode, ops: Vec<Op>, readdir_limit: usize) {
        let test_prefix = Prefix::new("test_prefix/").expect("valid prefix");
        let config = S3FilesystemConfig {
            readdir_size: 5,
            ..Default::default()
        };
        let (client, fs) = make_test_filesystem("harness", &test_prefix, config);

        let namespace = flatten_tree(initial_tree);
        for (key, object) in namespace.iter() {
            client.add_object(&format!("{test_prefix}{key}"), object.to_mock_object());
        }

        let reference = build_reference(namespace);

        let mut harness = Harness::new(fs, reference, readdir_limit);

        futures::executor::block_on(harness.run(ops));
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            .. ProptestConfig::default()
        })]

        #[test]
        fn reftest_random_tree(tree in gen_tree(5, 100, 5, 20), readdir_limit in 0..10usize, ops in vec(any::<Op>(), 1..10)) {
            run_test(tree, ops, readdir_limit);
        }
    }

    #[test]
    fn regression_basic() {
        run_test(
            TreeNode::Directory(BTreeMap::from([(
                Name("-".to_string()),
                TreeNode::Directory(BTreeMap::from([(
                    Name("-".to_string()),
                    TreeNode::File(FileContent(0, FileSize::Small(0))),
                )])),
            )])),
            vec![
                Op::WriteFile(
                    "a".to_string(),
                    DirectoryIndex(0),
                    FileContent(0x0a, FileSize::Small(50)),
                ),
                Op::WriteFile(
                    "b".to_string(),
                    DirectoryIndex(1),
                    FileContent(0x0b, FileSize::Small(10)),
                ),
            ],
            0,
        );
    }

    #[test]
    fn regression_overwrite() {
        run_test(
            TreeNode::File(FileContent(0, FileSize::Small(0))),
            vec![
                Op::WriteFile("-a".to_string(), DirectoryIndex(0), FileContent(0, FileSize::Small(0))),
                Op::WriteFile("-a".to_string(), DirectoryIndex(0), FileContent(0, FileSize::Small(0))),
            ],
            0,
        )
    }
}
