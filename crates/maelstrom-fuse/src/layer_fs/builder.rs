use crate::layer_fs::dir::{DirectoryDataReader, DirectoryDataWriter, OrderedDirectoryStream};
use crate::layer_fs::file::FileMetadataWriter;
use crate::layer_fs::ty::{
    DirectoryEntryData, FileAttributes, FileData, FileId, FileType, LayerId, LayerSuper,
};
use crate::layer_fs::LayerFs;
use anyhow::bail;
use anyhow::{anyhow, Result};
use futures::stream::{Peekable, StreamExt as _};
use maelstrom_base::Sha256Digest;
use maelstrom_base::{
    manifest::{Mode, UnixTimestamp},
    Utf8Component, Utf8Path,
};
use maelstrom_util::async_fs::Fs;
use maelstrom_util::ext::BoolExt as _;
use std::cmp::Ordering;
use std::path::Path;
use std::pin::Pin;
use tokio::io::AsyncRead;
use tokio_tar::{Archive, EntryType};

pub struct BottomLayerBuilder<'fs> {
    layer_fs: LayerFs,
    file_writer: FileMetadataWriter<'fs>,
    time: UnixTimestamp,
}

impl<'fs> BottomLayerBuilder<'fs> {
    pub async fn new(
        data_fs: &'fs Fs,
        data_dir: &Path,
        cache_path: &Path,
        time: UnixTimestamp,
    ) -> Result<Self> {
        let layer_fs = LayerFs::new(data_dir, cache_path, LayerSuper::default()).await?;
        let file_table_path = layer_fs.file_table_path(LayerId::BOTTOM).await?;
        let attribute_table_path = layer_fs.attributes_table_path(LayerId::BOTTOM).await?;

        let mut file_writer = FileMetadataWriter::new(
            data_fs,
            LayerId::BOTTOM,
            &file_table_path,
            &attribute_table_path,
        )
        .await?;
        let root = file_writer
            .insert_file(
                FileType::Directory,
                FileAttributes {
                    size: 0,
                    mode: Mode(0o777),
                    mtime: time,
                },
                FileData::Empty,
            )
            .await?;
        assert_eq!(root, FileId::root(LayerId::BOTTOM));
        DirectoryDataWriter::write_empty(&layer_fs, root).await?;
        file_writer.flush().await?;

        Ok(Self {
            layer_fs,
            file_writer,
            time,
        })
    }

    async fn look_up(&mut self, dir_id: FileId, name: &str) -> Result<Option<FileId>> {
        let mut dir_reader = DirectoryDataReader::new(&self.layer_fs, dir_id).await?;
        dir_reader.look_up(name).await
    }

    async fn look_up_entry(
        &mut self,
        dir_id: FileId,
        name: &str,
    ) -> Result<Option<DirectoryEntryData>> {
        let mut dir_reader = DirectoryDataReader::new(&self.layer_fs, dir_id).await?;
        dir_reader.look_up_entry(name).await
    }

    async fn ensure_path(&mut self, path: &Utf8Path) -> Result<FileId> {
        let comp_iter = path.components();

        let mut dir_id = FileId::root(LayerId::BOTTOM);
        for comp in comp_iter {
            if let Utf8Component::RootDir = comp {
                continue;
            };
            let Utf8Component::Normal(comp) = comp else {
                return Err(anyhow!("unsupported path {path}"));
            };
            match self.look_up(dir_id, comp).await? {
                Some(new_dir_id) => dir_id = new_dir_id,
                None => {
                    dir_id = {
                        let attrs = FileAttributes {
                            size: 0,
                            mode: Mode(0o777),
                            mtime: self.time,
                        };
                        self.add_dir(dir_id, comp, attrs).await?
                    }
                }
            }
        }
        Ok(dir_id)
    }

    async fn add_dir(
        &mut self,
        parent: FileId,
        name: &str,
        attrs: FileAttributes,
    ) -> Result<FileId> {
        let file_id = self
            .file_writer
            .insert_file(FileType::Directory, attrs, FileData::Empty)
            .await?;
        self.add_link(parent, name, file_id, FileType::Directory)
            .await?
            .assert_is_true();
        DirectoryDataWriter::write_empty(&self.layer_fs, file_id).await?;

        Ok(file_id)
    }

    async fn add_link(
        &mut self,
        parent: FileId,
        name: &str,
        file_id: FileId,
        kind: FileType,
    ) -> Result<bool> {
        let mut dir_writer = DirectoryDataWriter::new(&self.layer_fs, parent).await?;
        let inserted = dir_writer
            .insert_entry(name, DirectoryEntryData { file_id, kind })
            .await?;
        dir_writer.flush().await?;
        Ok(inserted)
    }

    pub async fn add_file_path(
        &mut self,
        path: &Utf8Path,
        attrs: FileAttributes,
        data: FileData,
    ) -> Result<FileId> {
        let file_id = self
            .file_writer
            .insert_file(FileType::RegularFile, attrs, data)
            .await?;

        let parent_id = if let Some(parent) = path.parent() {
            self.ensure_path(parent).await?
        } else {
            FileId::root(LayerId::BOTTOM)
        };
        let name = path.file_name().ok_or(anyhow!("missing file name"))?;
        let inserted = self
            .add_link(parent_id, name, file_id, FileType::RegularFile)
            .await?;
        if !inserted {
            return Err(anyhow!("file already exists at {path}"));
        }

        Ok(file_id)
    }

    pub async fn set_attr(&mut self, id: FileId, attrs: FileAttributes) -> Result<()> {
        self.file_writer.update_attributes(id, attrs).await
    }

    pub async fn add_dir_path(&mut self, path: &Utf8Path, attrs: FileAttributes) -> Result<FileId> {
        let parent_id = if let Some(parent) = path.parent() {
            self.ensure_path(parent).await?
        } else {
            FileId::root(LayerId::BOTTOM)
        };
        let name = path.file_name().ok_or(anyhow!("missing file name"))?;
        if let Some(existing) = self.look_up(parent_id, name).await? {
            self.set_attr(existing, attrs).await?;
            Ok(existing)
        } else {
            self.add_dir(parent_id, name, attrs).await
        }
    }

    pub async fn add_symlink_path(
        &mut self,
        path: &Utf8Path,
        target: impl Into<Vec<u8>>,
    ) -> Result<FileId> {
        let attrs = FileAttributes {
            size: 0,
            mode: Mode(0o777),
            mtime: self.time,
        };
        let file_id = self
            .file_writer
            .insert_file(FileType::Symlink, attrs, FileData::Inline(target.into()))
            .await?;

        let parent_id = if let Some(parent) = path.parent() {
            self.ensure_path(parent).await?
        } else {
            FileId::root(LayerId::BOTTOM)
        };
        let name = path.file_name().ok_or(anyhow!("missing file name"))?;
        let inserted = self
            .add_link(parent_id, name, file_id, FileType::RegularFile)
            .await?;
        if !inserted {
            return Err(anyhow!("file already exists at {path}"));
        }

        Ok(file_id)
    }

    pub async fn add_link_path(&mut self, path: &Utf8Path, target: &Utf8Path) -> Result<FileId> {
        let parent_id = if let Some(parent) = path.parent() {
            self.ensure_path(parent).await?
        } else {
            FileId::root(LayerId::BOTTOM)
        };
        let name = path.file_name().ok_or(anyhow!("missing file name"))?;

        let target_parent_id = if let Some(parent) = target.parent() {
            self.ensure_path(parent).await?
        } else {
            FileId::root(LayerId::BOTTOM)
        };
        let target_name = target.file_name().ok_or(anyhow!("missing file name"))?;

        let existing = self
            .look_up_entry(target_parent_id, target_name)
            .await?
            .ok_or(anyhow!("link target not found {target:?}"))?;

        if existing.kind == FileType::Directory {
            bail!("hardlink to directory not allowed {target:?}")
        }

        self.add_link(parent_id, name, existing.file_id, existing.kind)
            .await?;

        Ok(existing.file_id)
    }

    pub async fn add_from_tar(
        &mut self,
        digest: Sha256Digest,
        tar_stream: impl AsyncRead + Unpin,
    ) -> Result<()> {
        let mut ar = Archive::new(tar_stream);
        let mut entries = ar.entries()?;
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            let header = entry.header();
            let entry_path = entry.path()?;
            let utf8_path: &Utf8Path = entry_path
                .to_str()
                .ok_or(anyhow!("non-UTF8 path in tar"))?
                .as_ref();
            match header.entry_type() {
                EntryType::Regular => {
                    self.add_file_path(
                        &Utf8Path::new("/").join(utf8_path),
                        FileAttributes {
                            size: header.size()?,
                            mode: Mode(header.mode()?),
                            mtime: UnixTimestamp(header.mtime()?.try_into()?),
                        },
                        FileData::Digest {
                            digest: digest.clone(),
                            offset: entry.raw_file_position(),
                            length: header.entry_size()?,
                        },
                    )
                    .await?;
                }
                EntryType::Directory => {
                    self.add_dir_path(
                        &Utf8Path::new("/").join(utf8_path),
                        FileAttributes {
                            size: header.size()?,
                            mode: Mode(header.mode()?),
                            mtime: UnixTimestamp(header.mtime()?.try_into()?),
                        },
                    )
                    .await?;
                }
                EntryType::Symlink => {
                    self.add_symlink_path(
                        &Utf8Path::new("/").join(utf8_path),
                        header.link_name_bytes().expect("empty symlink in tar"),
                    )
                    .await?;
                }
                EntryType::Link => {
                    self.add_link_path(
                        &Utf8Path::new("/").join(utf8_path),
                        std::str::from_utf8(
                            &header.link_name_bytes().expect("empty symlink in tar"),
                        )?
                        .into(),
                    )
                    .await?;
                }
                other => {
                    bail!("unsupported tar entry type {other:?}")
                }
            }
        }
        self.file_writer.flush().await?;

        Ok(())
    }

    pub fn finish(self) -> LayerFs {
        self.layer_fs
    }
}

/// Walks the `right_fs` and yields together with it any matching entries from `left_fs`
pub struct DoubleFsWalk<'fs> {
    streams: Vec<(Option<WalkStream<'fs>>, WalkStream<'fs>)>,
    left_fs: &'fs LayerFs,
    right_fs: &'fs LayerFs,
}

#[derive(Debug)]
enum LeftRight<T> {
    Left(T),
    Right(T),
    Both(T, T),
}

struct WalkStream<'fs> {
    stream: Peekable<OrderedDirectoryStream<'fs>>,
    right_parent: FileId,
}

impl<'fs> WalkStream<'fs> {
    async fn new(fs: &'fs LayerFs, file_id: FileId, right_parent: FileId) -> Result<Self> {
        Ok(Self {
            stream: DirectoryDataReader::new(fs, file_id)
                .await?
                .into_ordered_stream()
                .await?
                .peekable(),
            right_parent,
        })
    }

    async fn next(&mut self) -> Result<Option<WalkEntry>> {
        Ok(self
            .stream
            .next()
            .await
            .transpose()?
            .map(|(key, data)| WalkEntry {
                key,
                data,
                right_parent: self.right_parent,
            }))
    }
}

#[derive(Debug)]
struct WalkEntry {
    key: String,
    data: DirectoryEntryData,
    right_parent: FileId,
}

#[allow(dead_code)]
impl<'fs> DoubleFsWalk<'fs> {
    async fn new(left_fs: &'fs LayerFs, right_fs: &'fs LayerFs) -> Result<Self> {
        let streams = vec![(
            Some(WalkStream::new(left_fs, left_fs.root().await?, right_fs.root().await?).await?),
            WalkStream::new(right_fs, right_fs.root().await?, right_fs.root().await?).await?,
        )];
        Ok(Self {
            streams,
            left_fs,
            right_fs,
        })
    }

    async fn next(&mut self) -> Result<Option<LeftRight<WalkEntry>>> {
        let res = loop {
            let Some((left, right)) = self.streams.last_mut() else {
                return Ok(None);
            };
            let Some(left) = left else {
                if let Some(entry) = right.next().await? {
                    break LeftRight::Right(entry);
                }
                self.streams.pop();
                continue;
            };

            let left_entry = Pin::new(&mut left.stream).peek().await;
            let right_entry = Pin::new(&mut right.stream).peek().await;

            break match (left_entry, right_entry) {
                (Some(_), None) | (Some(_), Some(Err(_))) => {
                    LeftRight::Left(left.next().await?.unwrap())
                }
                (None, Some(_)) | (Some(Err(_)), Some(_)) => {
                    LeftRight::Right(right.next().await?.unwrap())
                }
                (Some(Ok((left_key, _))), Some(Ok((right_key, _)))) => {
                    match left_key.cmp(right_key) {
                        Ordering::Less => LeftRight::Left(left.next().await?.unwrap()),
                        Ordering::Greater => LeftRight::Right(right.next().await?.unwrap()),
                        Ordering::Equal => LeftRight::Both(
                            left.next().await?.unwrap(),
                            right.next().await?.unwrap(),
                        ),
                    }
                }
                (None, None) => {
                    self.streams.pop();
                    continue;
                }
            };
        };

        match &res {
            LeftRight::Both(WalkEntry { data: left, .. }, WalkEntry { data: right, .. }) => {
                if left.kind == FileType::Directory && right.kind == FileType::Directory {
                    self.streams.push((
                        Some(WalkStream::new(self.left_fs, left.file_id, right.file_id).await?),
                        WalkStream::new(self.right_fs, right.file_id, right.file_id).await?,
                    ));
                } else if right.kind == FileType::Directory {
                    self.streams.push((
                        None,
                        WalkStream::new(self.right_fs, right.file_id, right.file_id).await?,
                    ));
                }
            }
            LeftRight::Right(WalkEntry { data: right, .. }) => {
                if right.kind == FileType::Directory {
                    self.streams.push((
                        None,
                        WalkStream::new(self.right_fs, right.file_id, right.file_id).await?,
                    ));
                }
            }
            _ => (),
        }

        Ok(Some(res))
    }
}

#[allow(dead_code)]
pub struct UpperLayerBuilder<'fs> {
    upper: LayerFs,
    lower: &'fs LayerFs,
}

#[allow(dead_code)]
impl<'fs> UpperLayerBuilder<'fs> {
    pub async fn new(data_dir: &Path, cache_dir: &Path, lower: &'fs LayerFs) -> Result<Self> {
        let lower_id = lower.layer_super().await?.layer_id;
        let upper_id = lower_id.inc();
        let mut upper_super = lower.layer_super().await?;
        upper_super.layer_id = upper_id;
        upper_super
            .lower_layers
            .insert(lower_id, lower.top_layer_path.clone());

        let upper = LayerFs::new(data_dir, cache_dir, upper_super).await?;

        Ok(Self { upper, lower })
    }

    async fn hard_link_files(&mut self, other: &LayerFs) -> Result<()> {
        let other_file_table = other
            .file_table_path(other.layer_super().await?.layer_id)
            .await?;
        let upper_file_table = self
            .upper
            .file_table_path(self.upper.layer_super().await?.layer_id)
            .await?;
        if self.upper.data_fs.exists(&upper_file_table).await {
            self.upper.data_fs.remove_file(&upper_file_table).await?;
        }
        self.upper
            .data_fs
            .hard_link(other_file_table, upper_file_table)
            .await?;

        let other_attribute_table = other
            .attributes_table_path(other.layer_super().await?.layer_id)
            .await?;
        let upper_attribute_table = self
            .upper
            .attributes_table_path(self.upper.layer_super().await?.layer_id)
            .await?;
        if self.upper.data_fs.exists(&upper_attribute_table).await {
            self.upper
                .data_fs
                .remove_file(&upper_attribute_table)
                .await?;
        }
        self.upper
            .data_fs
            .hard_link(other_attribute_table, upper_attribute_table)
            .await?;

        Ok(())
    }

    pub async fn fill_from_bottom_layer(&mut self, other: &LayerFs) -> Result<()> {
        self.hard_link_files(other).await?;
        let upper_id = self.upper.layer_super().await?.layer_id;
        let mut walker = DoubleFsWalk::new(self.lower, other).await?;
        while let Some(res) = walker.next().await? {
            match res {
                LeftRight::Left(entry) => {
                    let dir_id = FileId::new(upper_id, entry.right_parent.offset());
                    let mut writer = DirectoryDataWriter::new(&self.upper, dir_id).await?;
                    writer.insert_entry(&entry.key, entry.data).await?;
                    writer.flush().await?;
                }
                LeftRight::Right(mut entry) | LeftRight::Both(_, mut entry) => {
                    let dir_id = FileId::new(upper_id, entry.right_parent.offset());
                    let mut writer = DirectoryDataWriter::new(&self.upper, dir_id).await?;
                    let file_id = FileId::new(upper_id, entry.data.file_id.offset());
                    entry.data.file_id = file_id;
                    let kind = entry.data.kind;
                    writer.insert_entry(&entry.key, entry.data).await?;
                    writer.flush().await?;
                    if kind == FileType::Directory {
                        DirectoryDataWriter::write_empty(&self.upper, file_id).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish(self) -> LayerFs {
        self.upper
    }
}
