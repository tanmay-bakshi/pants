// Copyright 2022 Pants project contributors (see CONTRIBUTORS.md).
// Licensed under the Apache License, Version 2.0 (see LICENSE).
use super::{EntryType, ShrinkBehavior};

use std::collections::{BinaryHeap, HashSet};
use std::fmt::Debug;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::{self, join_all, try_join, try_join_all};
use hashing::{
  async_copy_and_hash, async_verified_copy, AgedFingerprint, Digest, Fingerprint, EMPTY_DIGEST,
};
use sharded_lmdb::ShardedLmdb;
use std::os::unix::fs::PermissionsExt;
use task_executor::Executor;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use workunit_store::ObservationMetric;

/// How big a file must be to be stored as a file on disk.
// NB: These numbers were chosen after micro-benchmarking the code on one machine at the time of
// writing. They were chosen using a rough equation from the microbenchmarks that are optimized
// for somewhere between 2 and 3 uses of the corresponding entry to "break even".
const LARGE_FILE_SIZE_LIMIT: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct TempImmutableLargeFile {
  tmp_path: PathBuf,
  final_path: PathBuf,
}

impl TempImmutableLargeFile {
  pub async fn open(&self) -> tokio::io::Result<tokio::fs::File> {
    tokio::fs::File::create(self.tmp_path.clone()).await
  }

  pub async fn persist(&self) -> Result<(), String> {
    tokio::fs::rename(self.tmp_path.clone(), self.final_path.clone())
      .await
      .map_err(|e| format!("Error while renaming: {e}."))?;
    tokio::fs::set_permissions(&self.final_path, std::fs::Permissions::from_mode(0o555))
      .await
      .map_err(|e| e.to_string())?;
    Ok(())
  }
}

/// Trait for the underlying storage, which is either a ShardedLMDB or a ShardedFS.
#[async_trait]
trait UnderlyingByteStore {
  async fn exists_batch(
    &self,
    fingerprints: Vec<Fingerprint>,
  ) -> Result<HashSet<Fingerprint>, String>;

  async fn exists(&self, fingerprint: Fingerprint) -> Result<bool, String> {
    let exists = self.exists_batch(vec![fingerprint]).await?;
    Ok(exists.contains(&fingerprint))
  }

  async fn lease(&self, fingerprint: Fingerprint) -> Result<(), String>;

  async fn remove(&self, fingerprint: Fingerprint) -> Result<bool, String>;

  async fn store_bytes_batch(
    &self,
    items: Vec<(Fingerprint, Bytes)>,
    initial_lease: bool,
  ) -> Result<(), String>;

  async fn store(
    &self,
    initial_lease: bool,
    src_is_immutable: bool,
    expected_digest: Digest,
    src: PathBuf,
  ) -> Result<(), String>;

  async fn load_bytes_with<
    T: Send + 'static,
    F: FnMut(&[u8]) -> Result<T, String> + Send + Sync + 'static,
  >(
    &self,
    fingerprint: Fingerprint,
    mut f: F,
  ) -> Result<Option<T>, String>;

  async fn aged_fingerprints(&self) -> Result<Vec<AgedFingerprint>, String>;

  async fn all_digests(&self) -> Result<Vec<Digest>, String> {
    let fingerprints = self.aged_fingerprints().await?;
    Ok(
      fingerprints
        .into_iter()
        .map(|fingerprint| Digest {
          hash: fingerprint.fingerprint,
          size_bytes: fingerprint.size_bytes,
        })
        .collect(),
    )
  }
}

#[async_trait]
impl UnderlyingByteStore for ShardedLmdb {
  async fn exists_batch(
    &self,
    fingerprints: Vec<Fingerprint>,
  ) -> Result<HashSet<Fingerprint>, String> {
    self.exists_batch(fingerprints).await
  }

  async fn lease(&self, fingerprint: Fingerprint) -> Result<(), String> {
    self.lease(fingerprint).await
  }

  async fn remove(&self, fingerprint: Fingerprint) -> Result<bool, String> {
    self.remove(fingerprint).await
  }

  async fn store_bytes_batch(
    &self,
    items: Vec<(Fingerprint, Bytes)>,
    initial_lease: bool,
  ) -> Result<(), String> {
    self.store_bytes_batch(items, initial_lease).await
  }
  async fn store(
    &self,
    initial_lease: bool,
    src_is_immutable: bool,
    expected_digest: Digest,
    src: PathBuf,
  ) -> Result<(), String> {
    self
      .store(
        initial_lease,
        src_is_immutable,
        expected_digest,
        move || std::fs::File::open(&src),
      )
      .await
  }

  async fn load_bytes_with<
    T: Send + 'static,
    F: FnMut(&[u8]) -> Result<T, String> + Send + Sync + 'static,
  >(
    &self,
    fingerprint: Fingerprint,
    f: F,
  ) -> Result<Option<T>, String> {
    self.load_bytes_with(fingerprint, f).await
  }

  async fn aged_fingerprints(&self) -> Result<Vec<AgedFingerprint>, String> {
    self.all_fingerprints().await
  }
}

// We shard so there isn't a plethora of entries in one single dir.
#[derive(Debug, Clone)]
pub(crate) struct ShardedFSDB {
  root: PathBuf,
  executor: Executor,
  lease_time: Duration,
}

impl ShardedFSDB {
  pub(crate) fn get_path(&self, fingerprint: Fingerprint) -> PathBuf {
    let hex = fingerprint.to_hex();
    self.root.join(hex.get(0..2).unwrap()).join(hex)
  }

  pub(crate) async fn get_tempfile(
    &self,
    fingerprint: Fingerprint,
  ) -> Result<TempImmutableLargeFile, String> {
    let dest_path = self.get_path(fingerprint);
    tokio::fs::create_dir_all(dest_path.parent().unwrap())
      .await
      .map_err(|e| format! {"Failed to create local store subdirectory {dest_path:?}: {e}"})?;

    let dest_path2 = dest_path.clone();
    // Make the tempfile in the same dir as the final file so that materializing the final file doesn't
    // have to worry about parent dirs.
    let named_temp_file = self
      .executor
      .spawn_blocking(
        move || {
          NamedTempFile::new_in(dest_path2.parent().unwrap())
            .map_err(|e| format!("Failed to create temp file: {e}"))
        },
        |e| Err(format!("temp file creation task failed: {e}")),
      )
      .await?;
    let (_, tmp_path) = named_temp_file.keep().map_err(|e| e.to_string())?;
    Ok(TempImmutableLargeFile {
      tmp_path,
      final_path: dest_path,
    })
  }
}

#[async_trait]
impl UnderlyingByteStore for ShardedFSDB {
  async fn exists_batch(
    &self,
    fingerprints: Vec<Fingerprint>,
  ) -> Result<HashSet<Fingerprint>, String> {
    let results = join_all(
      fingerprints
        .iter()
        .map(|fingerprint| tokio::fs::metadata(self.get_path(*fingerprint))),
    )
    .await;
    let existing = results
      .iter()
      .zip(fingerprints)
      .filter_map(|(result, fingerprint)| {
        if result.is_ok() {
          Some(fingerprint)
        } else {
          None
        }
      })
      .collect::<Vec<_>>();

    Ok(HashSet::from_iter(existing))
  }

  async fn lease(&self, fingerprint: Fingerprint) -> Result<(), String> {
    let path = self.get_path(fingerprint);
    self
      .executor
      .spawn_blocking(
        move || {
          fs_set_times::set_mtime(&path, fs_set_times::SystemTimeSpec::SymbolicNow)
            .map_err(|e| format!("Failed to extend mtime of {path:?}: {e}"))
        },
        |e| Err(format!("`lease` task failed: {e}")),
      )
      .await
  }

  async fn remove(&self, fingerprint: Fingerprint) -> Result<bool, String> {
    Ok(
      tokio::fs::remove_file(self.get_path(fingerprint))
        .await
        .is_ok(),
    )
  }

  async fn store_bytes_batch(
    &self,
    items: Vec<(Fingerprint, Bytes)>,
    _initial_lease: bool,
  ) -> Result<(), String> {
    try_join_all(items.iter().map(|(fingerprint, bytes)| async move {
      let tempfile = self.get_tempfile(*fingerprint).await?;
      let mut dest = tempfile
        .open()
        .await
        .map_err(|e| format!("Failed to open {tempfile:?}: {e}"))?;
      dest.write_all(bytes).await.map_err(|e| e.to_string())?;
      tempfile.persist().await?;
      Ok::<(), String>(())
    }))
    .await?;

    Ok(())
  }

  async fn store(
    &self,
    _initial_lease: bool,
    src_is_immutable: bool,
    expected_digest: Digest,
    src: PathBuf,
  ) -> Result<(), String> {
    let dest = self.get_tempfile(expected_digest.hash).await?;
    let mut attempts = 0;
    loop {
      let (mut reader, mut writer) = try_join(tokio::fs::File::open(src.clone()), dest.open())
        .await
        .map_err(|e| e.to_string())?;
      // TODO: Consider using `fclonefileat` on macOS, which would skip actual copying (read+write), and
      // instead just require verifying the resulting content after the syscall (read only).
      let should_retry =
        !async_verified_copy(expected_digest, src_is_immutable, &mut reader, &mut writer)
          .await
          .map_err(|e| e.to_string())?;

      if should_retry {
        attempts += 1;
        let msg = format!("Input {src:?} changed while reading.");
        log::debug!("{}", msg);
        if attempts > 10 {
          return Err(format!("Failed to store {src:?}."));
        }
      } else {
        writer.flush().await.map_err(|e| e.to_string())?;
        dest.persist().await?;
        break;
      }
    }

    Ok(())
  }

  async fn load_bytes_with<
    T: Send + 'static,
    F: FnMut(&[u8]) -> Result<T, String> + Send + Sync + 'static,
  >(
    &self,
    fingerprint: Fingerprint,
    mut f: F,
  ) -> Result<Option<T>, String> {
    if let Ok(mut file) = tokio::fs::File::open(self.get_path(fingerprint)).await {
      // TODO: Use mmap instead of copying into user-space.
      let mut contents: Vec<u8> = vec![];
      file
        .read_to_end(&mut contents)
        .await
        .map_err(|e| format!("Failed to load large file into memory: {e}"))?;
      Ok(Some(f(&contents[..])?))
    } else {
      Ok(None)
    }
  }

  async fn aged_fingerprints(&self) -> Result<Vec<AgedFingerprint>, String> {
    // NB: The ShardLmdb implementation stores a lease time in the future, and then compares the
    // current time to the stored lease time for a fingerprint to determine how long ago it
    // expired. Rather than setting `mtimes` in the future, this implementation instead considers a
    // file to be expired if its mtime is outside of the lease time window.
    let root = self.root.clone();
    let expiration_time = SystemTime::now() - self.lease_time;
    self
      .executor
      .spawn_blocking(
        move || {
          let maybe_shards = std::fs::read_dir(&root);
          let mut fingerprints = vec![];
          if let Ok(shards) = maybe_shards {
            for entry in shards {
              let shard = entry.map_err(|e| format!("Error iterating dir {root:?}: {e}."))?;
              let large_files = std::fs::read_dir(shard.path())
                .map_err(|e| format!("Failed to read shard directory: {e}."))?;
              for entry in large_files {
                let large_file = entry.map_err(|e| {
                  format!("Error iterating dir {:?}: {e}", shard.path().file_name())
                })?;
                let path = large_file.path();
                let hash = path.file_name().unwrap().to_str().unwrap();
                let (length, mtime) = large_file
                  .metadata()
                  .and_then(|metadata| {
                    let length = metadata.len();
                    let mtime = metadata.modified()?;
                    Ok((length, mtime))
                  })
                  .map_err(|e| format!("Could not access metadata for {path:?}: {e}"))?;

                let expired_seconds_ago = expiration_time
                  .duration_since(mtime)
                  .map(|t| t.as_secs())
                  // 0 indicates unexpired.
                  .unwrap_or(0);

                fingerprints.push(AgedFingerprint {
                  expired_seconds_ago,
                  fingerprint: Fingerprint::from_hex_string(hash)
                    .map_err(|e| format!("Invalid file store entry at {path:?}: {e}"))?,
                  size_bytes: length as usize,
                });
              }
            }
          }
          Ok(fingerprints)
        },
        |e| Err(format!("`aged_fingerprints` task failed: {e}")),
      )
      .await
  }
}

#[derive(Debug, Clone)]
pub struct ByteStore {
  inner: Arc<InnerStore>,
}

#[derive(Debug)]
struct InnerStore {
  // Store directories separately from files because:
  //  1. They may have different lifetimes.
  //  2. It's nice to know whether we should be able to parse something as a proto.
  file_lmdb: Result<Arc<ShardedLmdb>, String>,
  directory_lmdb: Result<Arc<ShardedLmdb>, String>,
  file_fsdb: ShardedFSDB,
  executor: task_executor::Executor,
  filesystem_device: u64,
}

impl ByteStore {
  pub fn new<P: AsRef<Path>>(
    executor: task_executor::Executor,
    path: P,
  ) -> Result<ByteStore, String> {
    Self::new_with_options(executor, path, super::LocalOptions::default())
  }

  pub fn new_with_options<P: AsRef<Path>>(
    executor: task_executor::Executor,
    path: P,
    options: super::LocalOptions,
  ) -> Result<ByteStore, String> {
    let root = path.as_ref();
    let lmdb_files_root = root.join("files");
    let lmdb_directories_root = root.join("directories");
    let fsdb_files_root = root.join("immutable").join("files");

    fs::safe_create_dir_all(path.as_ref())?;

    let filesystem_device = root
      .metadata()
      .map_err(|e| {
        format!(
          "Failed to get metadata for store root {}: {e}",
          root.display()
        )
      })?
      .dev();

    Ok(ByteStore {
      inner: Arc::new(InnerStore {
        file_lmdb: ShardedLmdb::new(
          lmdb_files_root,
          options.files_max_size_bytes,
          executor.clone(),
          options.lease_time,
          options.shard_count,
        )
        .map(Arc::new),
        directory_lmdb: ShardedLmdb::new(
          lmdb_directories_root,
          options.directories_max_size_bytes,
          executor.clone(),
          options.lease_time,
          options.shard_count,
        )
        .map(Arc::new),
        file_fsdb: ShardedFSDB {
          executor: executor.clone(),
          root: fsdb_files_root,
          lease_time: options.lease_time,
        },
        executor,
        filesystem_device,
      }),
    })
  }

  pub fn executor(&self) -> &task_executor::Executor {
    &self.inner.executor
  }

  pub fn filesystem_device(&self) -> u64 {
    self.inner.filesystem_device
  }

  pub async fn entry_type(&self, fingerprint: Fingerprint) -> Result<Option<EntryType>, String> {
    if fingerprint == EMPTY_DIGEST.hash {
      // Technically this is valid as both; choose Directory in case a caller is checking whether
      // it _can_ be a Directory.
      return Ok(Some(EntryType::Directory));
    }

    // In parallel, check for the given fingerprint in all databases.
    let directory_lmdb = self.inner.directory_lmdb.clone()?;
    let is_lmdb_dir = directory_lmdb.exists(fingerprint);
    let file_lmdb = self.inner.file_lmdb.clone()?;
    let is_lmdb_file = file_lmdb.exists(fingerprint);
    let is_fsdb_file = self.inner.file_fsdb.exists(fingerprint);

    // TODO: Could technically use select to return slightly more quickly with the first
    // affirmative answer, but this is simpler.
    match future::try_join3(is_lmdb_dir, is_lmdb_file, is_fsdb_file).await? {
      (true, _, _) => Ok(Some(EntryType::Directory)),
      (_, true, _) => Ok(Some(EntryType::File)),
      (_, _, true) => Ok(Some(EntryType::File)),
      (false, false, false) => Ok(None),
    }
  }

  pub async fn lease_all(
    &self,
    digests: impl Iterator<Item = (Digest, EntryType)>,
  ) -> Result<(), String> {
    // NB: Lease extension happens periodically in the background, so this code needn't be parallel.
    for (digest, entry_type) in digests {
      if ByteStore::should_use_fsdb(entry_type, digest.size_bytes) {
        self.inner.file_fsdb.lease(digest.hash).await?;
      } else {
        let dbs = match entry_type {
          EntryType::File => self.inner.file_lmdb.clone(),
          EntryType::Directory => self.inner.directory_lmdb.clone(),
        };
        dbs?
          .lease(digest.hash)
          .await
          .map_err(|err| format!("Error leasing digest {digest:?}: {err}"))?;
      }
    }
    Ok(())
  }

  ///
  /// Attempts to shrink the stored files to be no bigger than target_bytes
  /// (excluding lmdb overhead).
  ///
  /// Returns the size it was shrunk to, which may be larger than target_bytes.
  ///
  /// TODO: Use LMDB database statistics when lmdb-rs exposes them.
  ///
  pub async fn shrink(
    &self,
    target_bytes: usize,
    shrink_behavior: ShrinkBehavior,
  ) -> Result<usize, String> {
    let mut used_bytes: usize = 0;
    let mut fingerprints_by_expired_ago = BinaryHeap::new();

    fingerprints_by_expired_ago.extend(
      self
        .inner
        .file_lmdb
        .clone()?
        .aged_fingerprints()
        .await?
        .into_iter()
        .map(|fingerprint| {
          used_bytes += fingerprint.size_bytes;
          (fingerprint, EntryType::File)
        }),
    );
    fingerprints_by_expired_ago.extend(
      self
        .inner
        .directory_lmdb
        .clone()?
        .aged_fingerprints()
        .await?
        .into_iter()
        .map(|fingerprint| {
          used_bytes += fingerprint.size_bytes;
          (fingerprint, EntryType::Directory)
        }),
    );
    fingerprints_by_expired_ago.extend(
      self
        .inner
        .file_fsdb
        .aged_fingerprints()
        .await?
        .into_iter()
        .map(|fingerprint| {
          used_bytes += fingerprint.size_bytes;
          (fingerprint, EntryType::File)
        }),
    );

    while used_bytes > target_bytes {
      let (aged_fingerprint, entry_type) = fingerprints_by_expired_ago
        .pop()
        .expect("lmdb corruption detected, sum of size of blobs exceeded stored blobs");
      if aged_fingerprint.expired_seconds_ago == 0 {
        // Ran out of expired blobs - everything remaining is leased and cannot be collected.
        return Ok(used_bytes);
      }
      self
        .remove(
          entry_type,
          Digest {
            hash: aged_fingerprint.fingerprint,
            size_bytes: aged_fingerprint.size_bytes,
          },
        )
        .await?;
      used_bytes -= aged_fingerprint.size_bytes;
    }

    if shrink_behavior == ShrinkBehavior::Compact {
      self.inner.file_lmdb.clone()?.compact()?;
    }

    Ok(used_bytes)
  }

  pub async fn remove(&self, entry_type: EntryType, digest: Digest) -> Result<bool, String> {
    match entry_type {
      EntryType::Directory => self.inner.directory_lmdb.clone()?.remove(digest.hash).await,
      EntryType::File if ByteStore::should_use_fsdb(entry_type, digest.size_bytes) => {
        self.inner.file_fsdb.remove(digest.hash).await
      }
      EntryType::File => self.inner.file_lmdb.clone()?.remove(digest.hash).await,
    }
  }

  ///
  /// Store the given data in a single pass, using the given Fingerprint. Prefer `Self::store`
  /// for values which should not be pulled into memory, and `Self::store_bytes_batch` when storing
  /// multiple values at a time.
  ///
  pub async fn store_bytes(
    &self,
    entry_type: EntryType,
    fingerprint: Fingerprint,
    bytes: Bytes,
    initial_lease: bool,
  ) -> Result<(), String> {
    self
      .store_bytes_batch(entry_type, vec![(fingerprint, bytes)], initial_lease)
      .await
  }

  ///
  /// Store the given items in a single pass, optionally using the given Digests. Prefer `Self::store`
  /// for values which should not be pulled into memory.
  ///
  /// See also: `Self::store_bytes`.
  ///
  pub async fn store_bytes_batch(
    &self,
    entry_type: EntryType,
    items: Vec<(Fingerprint, Bytes)>,
    initial_lease: bool,
  ) -> Result<(), String> {
    let mut fsdb_items = vec![];
    let mut lmdb_items = vec![];
    for (fingerprint, bytes) in items {
      if ByteStore::should_use_fsdb(entry_type, bytes.len()) {
        fsdb_items.push((fingerprint, bytes));
      } else {
        lmdb_items.push((fingerprint, bytes));
      }
    }

    let lmdb_dbs = match entry_type {
      EntryType::Directory => self.inner.directory_lmdb.clone(),
      EntryType::File => self.inner.file_lmdb.clone(),
    };
    try_join(
      self
        .inner
        .file_fsdb
        .store_bytes_batch(fsdb_items, initial_lease),
      lmdb_dbs?.store_bytes_batch(lmdb_items, initial_lease),
    )
    .await?;

    Ok(())
  }

  ///
  /// Store data in two passes, without buffering it entirely into memory. Prefer
  /// `Self::store_bytes` for small values which fit comfortably in memory.
  ///
  pub async fn store(
    &self,
    entry_type: EntryType,
    initial_lease: bool,
    src_is_immutable: bool,
    src: PathBuf,
  ) -> Result<Digest, String> {
    let mut file = tokio::fs::File::open(src.clone())
      .await
      .map_err(|e| format!("Failed to open {src:?}: {e}"))?;
    let digest = async_copy_and_hash(&mut file, &mut tokio::io::sink())
      .await
      .map_err(|e| format!("Failed to hash {src:?}: {e}"))?;

    if ByteStore::should_use_fsdb(entry_type, digest.size_bytes) {
      self
        .inner
        .file_fsdb
        .store(initial_lease, src_is_immutable, digest, src)
        .await?;
    } else {
      let dbs = match entry_type {
        EntryType::Directory => self.inner.directory_lmdb.clone()?,
        EntryType::File => self.inner.file_lmdb.clone()?,
      };
      let _ = dbs
        .store(initial_lease, src_is_immutable, digest, move || {
          std::fs::File::open(&src)
        })
        .await;
    }

    Ok(digest)
  }

  ///
  /// Given a collection of Digests (digests),
  /// returns the set of digests from that collection not present in the
  /// underlying LMDB store.
  ///
  pub async fn get_missing_digests(
    &self,
    entry_type: EntryType,
    digests: HashSet<Digest>,
  ) -> Result<HashSet<Digest>, String> {
    let mut fsdb_digests = vec![];
    let mut lmdb_digests = vec![];
    for digest in digests.iter() {
      if ByteStore::should_use_fsdb(entry_type, digest.size_bytes) {
        fsdb_digests.push(digest);
      }
      // Avoid I/O for this case. This allows some client-provided operations (like
      // merging snapshots) to work without needing to first store the empty snapshot.
      else if *digest != EMPTY_DIGEST {
        lmdb_digests.push(digest);
      }
    }

    let lmdb = match entry_type {
      EntryType::Directory => self.inner.directory_lmdb.clone(),
      EntryType::File => self.inner.file_lmdb.clone(),
    }?;
    let (mut existing, existing_lmdb_digests) = try_join(
      self
        .inner
        .file_fsdb
        .exists_batch(fsdb_digests.iter().map(|digest| digest.hash).collect()),
      lmdb.exists_batch(lmdb_digests.iter().map(|digest| digest.hash).collect()),
    )
    .await?;

    existing.extend(existing_lmdb_digests);

    Ok(
      digests
        .into_iter()
        .filter(|digest| *digest != EMPTY_DIGEST && !existing.contains(&digest.hash))
        .collect(),
    )
  }

  ///
  /// Return the path this digest is persistent on the filesystem at, or None.
  ///
  pub async fn load_from_fs(&self, digest: Digest) -> Result<Option<PathBuf>, String> {
    if self.inner.file_fsdb.exists(digest.hash).await? {
      return Ok(Some(self.inner.file_fsdb.get_path(digest.hash)));
    }
    Ok(None)
  }

  ///
  /// Loads bytes from the underlying store using the given function.
  /// In the case of the LMDB store, because the database is blocking, this accepts a function that
  /// views a slice rather than returning a clone of the data.
  /// The upshot is that the database is able to provide slices directly into shared memory.
  ///
  pub async fn load_bytes_with<T: Send + 'static, F: FnMut(&[u8]) -> T + Send + Sync + 'static>(
    &self,
    entry_type: EntryType,
    digest: Digest,
    mut f: F,
  ) -> Result<Option<T>, String> {
    let start = Instant::now();
    if digest == EMPTY_DIGEST {
      // Avoid I/O for this case. This allows some client-provided operations (like merging
      // snapshots) to work without needing to first store the empty snapshot.
      return Ok(Some(f(&[])));
    }

    let len_checked_f = move |bytes: &[u8]| {
      if bytes.len() == digest.size_bytes {
        Ok(f(bytes))
      } else {
        Err(format!(
          "Got hash collision reading from store - digest {:?} was requested, but retrieved \
                bytes with that fingerprint had length {}. Congratulations, you may have broken \
                sha256! Underlying bytes: {:?}",
          digest,
          bytes.len(),
          bytes
        ))
      }
    };

    let result = if ByteStore::should_use_fsdb(entry_type, digest.size_bytes) {
      self
        .inner
        .file_fsdb
        .load_bytes_with(digest.hash, len_checked_f)
        .await?
    } else {
      let dbs = match entry_type {
        EntryType::Directory => self.inner.directory_lmdb.clone(),
        EntryType::File => self.inner.file_lmdb.clone(),
      }?;
      dbs.load_bytes_with(digest.hash, len_checked_f).await?
    };

    if let Some(workunit_store_handle) = workunit_store::get_workunit_store_handle() {
      workunit_store_handle.store.record_observation(
        ObservationMetric::LocalStoreReadBlobSize,
        digest.size_bytes as u64,
      );
      workunit_store_handle.store.record_observation(
        ObservationMetric::LocalStoreReadBlobTimeMicros,
        start.elapsed().as_micros() as u64,
      );
    }

    Ok(result)
  }

  pub async fn all_digests(&self, entry_type: EntryType) -> Result<Vec<Digest>, String> {
    let lmdb = match entry_type {
      EntryType::File => self.inner.file_lmdb.clone(),
      EntryType::Directory => self.inner.directory_lmdb.clone(),
    }?;
    let mut digests = vec![];
    digests.extend(lmdb.all_digests().await?);
    digests.extend(self.inner.file_fsdb.all_digests().await?);
    Ok(digests)
  }

  pub(crate) fn should_use_fsdb(entry_type: EntryType, len: usize) -> bool {
    entry_type == EntryType::File && len >= LARGE_FILE_SIZE_LIMIT
  }

  pub(crate) fn get_file_fsdb(&self) -> ShardedFSDB {
    self.inner.file_fsdb.clone()
  }
}
