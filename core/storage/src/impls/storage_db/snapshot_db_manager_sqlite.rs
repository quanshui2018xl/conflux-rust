// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

pub struct SnapshotDbManagerSqlite {
    snapshot_path: PathBuf,
    // FIXME: add an command line option to assert that this method made
    // successfully cow_copy and print error messages if it fails.
    force_cow: bool,
    already_open_snapshots: AlreadyOpenSnapshots<SnapshotDbSqlite>,
    /// Set a limit on the number of open snapshots. When the limit is reached,
    /// consensus initiated open should wait, other non-critical opens such as
    /// rpc initiated opens should simply abort when the limit is reached.
    open_snapshot_semaphore: Arc<Semaphore>,
    open_create_delete_lock: Mutex<()>,
    use_isolated_db_for_mpt_table: bool,
    use_isolated_db_for_mpt_table_height: Option<u64>,
    mpt_snapshot_path: PathBuf,
    mpt_already_open_snapshots:
        AlreadyOpenSnapshots<RwLock<SnapshotMptDbSqlite>>,
    mpt_open_snapshot_semaphore: Arc<Semaphore>,
    era_epoch_count: u64,
    max_open_snapshots: u16,
    lastest_mpt_snapshot_semaphore: Arc<Semaphore>,
    latest_snapshot_id: RwLock<(EpochId, u64)>,
}

#[derive(Debug)]
enum CopyType {
    Cow,
    Std,
}

// The map from path to the already open snapshots.
// when the mapped snapshot is None, the snapshot is open exclusively for write,
// when the mapped snapshot is Some(), the snapshot can be shared by other
// readers.
pub type AlreadyOpenSnapshots<T> =
    Arc<RwLock<HashMap<PathBuf, Option<Weak<T>>>>>;

impl SnapshotDbManagerSqlite {
    pub const LATEST_MPT_SNAPSHOT_DIR: &'static str = "latest";
    const MPT_SNAPSHOT_DIR: &'static str = "mpt_snapshot";
    const SNAPSHOT_DB_SQLITE_DIR_PREFIX: &'static str = "sqlite_";

    pub fn new(
        snapshot_path: PathBuf, max_open_snapshots: u16,
        use_isolated_db_for_mpt_table: bool,
        use_isolated_db_for_mpt_table_height: Option<u64>,
        era_epoch_count: u64,
    ) -> Result<Self>
    {
        if !snapshot_path.exists() {
            fs::create_dir_all(snapshot_path.clone())?;
        }

        let mpt_snapshot_path = snapshot_path
            .parent()
            .unwrap()
            .join(SnapshotDbManagerSqlite::MPT_SNAPSHOT_DIR);
        let latest_mpt_snapshot_path = mpt_snapshot_path.join(
            Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
                + SnapshotDbManagerSqlite::LATEST_MPT_SNAPSHOT_DIR,
        );

        // Create the latest MPT database if database not exist
        SnapshotMptDbSqlite::create(
            latest_mpt_snapshot_path.as_path(),
            &Default::default(),
            &Arc::new(Semaphore::new(max_open_snapshots as usize)),
            None,
        )?;

        Ok(Self {
            snapshot_path,
            force_cow: false,
            already_open_snapshots: Default::default(),
            open_snapshot_semaphore: Arc::new(Semaphore::new(
                max_open_snapshots as usize,
            )),
            open_create_delete_lock: Default::default(),
            mpt_snapshot_path,
            use_isolated_db_for_mpt_table,
            use_isolated_db_for_mpt_table_height,
            mpt_already_open_snapshots: Default::default(),
            mpt_open_snapshot_semaphore: Arc::new(Semaphore::new(
                max_open_snapshots as usize,
            )),
            era_epoch_count,
            max_open_snapshots,
            lastest_mpt_snapshot_semaphore: Arc::new(Semaphore::new(1)),
            latest_snapshot_id: RwLock::new((NULL_EPOCH, 0)),
        })
    }

    pub fn update_latest_snapshot_id(&self, snapshot_id: EpochId, height: u64) {
        *self.latest_snapshot_id.write() = (snapshot_id, height);
    }

    fn open_snapshot_readonly(
        &self, snapshot_path: PathBuf, try_open: bool,
        snapshot_epoch_id: &EpochId, read_mpt_snapshot: bool,
    ) -> Result<Option<Arc<SnapshotDbSqlite>>>
    {
        if let Some(already_open) =
            self.already_open_snapshots.read().get(&snapshot_path)
        {
            match already_open {
                None => {
                    // Already open for exclusive write
                    return Ok(None);
                }
                Some(open_shared_weak) => {
                    match Weak::upgrade(open_shared_weak) {
                        None => {}
                        Some(already_open) => {
                            return Ok(Some(already_open));
                        }
                    }
                }
            }
        }
        let file_exists = snapshot_path.exists();
        if file_exists {
            let semaphore_permit = if try_open {
                self.open_snapshot_semaphore
                    .try_acquire()
                    // Unfortunately we have to use map_error because the
                    // TryAcquireError isn't public.
                    .map_err(|_err| ErrorKind::SemaphoreTryAcquireError)?
            } else {
                executor::block_on(self.open_snapshot_semaphore.acquire())
            };

            // To serialize simultaneous opens.
            let _open_lock = self.open_create_delete_lock.lock();
            // If it's not in already_open_snapshots, the sqlite db must have
            // been closed.
            while let Some(already_open) =
                self.already_open_snapshots.read().get(&snapshot_path)
            {
                match already_open {
                    None => {
                        // Already open for exclusive write
                        return Ok(None);
                    }
                    Some(open_shared_weak) => {
                        match Weak::upgrade(open_shared_weak) {
                            None => {
                                // All `Arc` of the sqlite db have been dropped,
                                // but the inner
                                // struct (sqlite db itself) drop is called
                                // after decreasing
                                // the strong_ref count, so it may still be open
                                // at this time,
                                // and after it's closed it will be removed from
                                // `already_open_snapshots`.
                                // Thus, here we wait for it to be removed to
                                // ensure that when we try
                                // to open it, it's guaranteed to be closed.
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            Some(already_open) => {
                                return Ok(Some(already_open));
                            }
                        }
                    }
                }
            }

            let mut snapshot_db = SnapshotDbSqlite::open(
                snapshot_path.as_path(),
                /* readonly = */ true,
                &self.already_open_snapshots,
                &self.open_snapshot_semaphore,
                None,
            )?;

            if !snapshot_db.is_mpt_table_in_current_db() && read_mpt_snapshot {
                // Use the existing directory for the specific database (for ear
                // checkpoint)
                let mpt_snapshot_path = if self.use_isolated_db_for_mpt_table {
                    let mpt_snapshot_path =
                        self.get_mpt_snapshot_db_path(snapshot_epoch_id);
                    if mpt_snapshot_path.exists() {
                        mpt_snapshot_path
                    } else {
                        if self.latest_snapshot_id.read().0
                            == *snapshot_epoch_id
                        {
                            self.get_latest_mpt_snapshot_db_path()
                        } else {
                            bail!("MPT DB not exist, latest snapshot id {:?}, try to open {:?}.", self.latest_snapshot_id.read().0, snapshot_epoch_id);
                        }
                    }
                } else {
                    bail!("MPT table should be in snapshot database.");
                };

                let mpt_snapshot_db = self.open_mpt_snapshot_readonly(
                    mpt_snapshot_path,
                    try_open,
                    snapshot_epoch_id,
                )?;

                snapshot_db.update_mpt_snapshot(mpt_snapshot_db);
            }

            let snapshot_db = Arc::new(snapshot_db);
            semaphore_permit.forget();
            self.already_open_snapshots.write().insert(
                snapshot_path.into(),
                Some(Arc::downgrade(&snapshot_db)),
            );

            return Ok(Some(snapshot_db));
        } else {
            return Ok(None);
        }
    }

    fn open_snapshot_write(
        &self, snapshot_path: PathBuf, create: bool, new_epoch_height: u64,
        mpt_snapshot_path: Option<PathBuf>, new_snapshot_id: &EpochId,
    ) -> Result<SnapshotDbSqlite>
    {
        if self
            .already_open_snapshots
            .read()
            .get(&snapshot_path)
            .is_some()
        {
            bail!(ErrorKind::SnapshotAlreadyExists)
        }

        let semaphore_permit =
            executor::block_on(self.open_snapshot_semaphore.acquire());
        // When an open happens around the same time, we should make sure that
        // the open returns None.
        let mut _open_lock = self.open_create_delete_lock.lock();

        // Simultaneous creation fails here.
        if self
            .already_open_snapshots
            .read()
            .get(&snapshot_path)
            .is_some()
        {
            bail!(ErrorKind::SnapshotAlreadyExists)
        }

        let mpt_table_in_current_db =
            self.is_mpt_table_in_current_db_for_epoch(new_epoch_height);

        let latest_mpt_snapshot = if mpt_table_in_current_db {
            None
        } else {
            let (mpt_snapshot_path, create_mpt) = match mpt_snapshot_path {
                Some(v) => (v, true),
                _ => {
                    assert!(
                        new_epoch_height > self.latest_snapshot_id.read().1,
                        "Try to write an old snapshot"
                    );
                    (self.get_latest_mpt_snapshot_db_path(), false)
                }
            };

            Some(self.open_mpt_snapshot_write(
                mpt_snapshot_path,
                create_mpt,
                new_epoch_height,
                new_snapshot_id,
            )?)
        };

        let snapshot_db = if create {
            SnapshotDbSqlite::create(
                snapshot_path.as_path(),
                &self.already_open_snapshots,
                &self.open_snapshot_semaphore,
                latest_mpt_snapshot,
                mpt_table_in_current_db,
            )
        } else {
            let file_exists = snapshot_path.exists();
            if file_exists {
                let mut db = SnapshotDbSqlite::open(
                    snapshot_path.as_path(),
                    /* readonly = */ false,
                    &self.already_open_snapshots,
                    &self.open_snapshot_semaphore,
                    latest_mpt_snapshot,
                )?;

                if !mpt_table_in_current_db {
                    db.drop_mpt_table()?;
                }

                Ok(db)
            } else {
                bail!(ErrorKind::SnapshotNotFound);
            }
        }?;

        semaphore_permit.forget();
        self.already_open_snapshots
            .write()
            .insert(snapshot_path.clone(), None);
        Ok(snapshot_db)
    }

    fn open_mpt_snapshot_readonly(
        &self, snapshot_path: PathBuf, try_open: bool,
        snapshot_epoch_id: &EpochId,
    ) -> Result<Option<Arc<RwLock<SnapshotMptDbSqlite>>>>
    {
        debug!(
            "Open mpt snapshot with readonly {:?}, snapshot_epoch_id {:?}",
            snapshot_path, snapshot_epoch_id
        );
        if let Some(already_open) =
            self.mpt_already_open_snapshots.read().get(&snapshot_path)
        {
            match already_open {
                None => {
                    // Already open for exclusive write
                    return Ok(None);
                }
                Some(open_shared_weak) => {
                    match Weak::upgrade(open_shared_weak) {
                        None => {}
                        Some(already_open) => {
                            return Ok(Some(already_open));
                        }
                    }
                }
            }
        }
        let file_exists = snapshot_path.exists();
        if file_exists {
            let semaphore_permit = if try_open {
                self.mpt_open_snapshot_semaphore
                    .try_acquire()
                    // Unfortunately we have to use map_error because the
                    // TryAcquireError isn't public.
                    .map_err(|_err| ErrorKind::SemaphoreTryAcquireError)?
            } else {
                executor::block_on(self.mpt_open_snapshot_semaphore.acquire())
            };

            // If it's not in already_open_snapshots, the sqlite db must have
            // been closed.
            while let Some(already_open) =
                self.mpt_already_open_snapshots.read().get(&snapshot_path)
            {
                match already_open {
                    None => {
                        // Already open for exclusive write
                        return Ok(None);
                    }
                    Some(open_shared_weak) => {
                        match Weak::upgrade(open_shared_weak) {
                            None => {
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            Some(already_open) => {
                                return Ok(Some(already_open));
                            }
                        }
                    }
                }
            }

            let (latest_mpt_semaphore_permit, v) = if self
                .latest_snapshot_id
                .read()
                .0
                == *snapshot_epoch_id
                && self.latest_snapshot_id.read().1 % self.era_epoch_count != 0
            {
                let s =
                    self.lastest_mpt_snapshot_semaphore.try_acquire().map_err(
                        |_err| "The MPT snapshot is already open for writing.",
                    )?;

                (Some(s), Some(self.lastest_mpt_snapshot_semaphore.clone()))
            } else {
                (None, None)
            };

            let snapshot_db = Arc::new(RwLock::new(SnapshotMptDbSqlite::open(
                snapshot_path.as_path(),
                /* readonly = */ true,
                &self.mpt_already_open_snapshots,
                &self.mpt_open_snapshot_semaphore,
                v,
            )?));

            if let Some(s) = latest_mpt_semaphore_permit {
                s.forget();
            }
            semaphore_permit.forget();
            self.mpt_already_open_snapshots.write().insert(
                snapshot_path.into(),
                Some(Arc::downgrade(&snapshot_db)),
            );

            return Ok(Some(snapshot_db));
        } else {
            return Ok(None);
        }
    }

    fn open_mpt_snapshot_write(
        &self, snapshot_path: PathBuf, create: bool, new_epoch_height: u64,
        new_snapshot_id: &EpochId,
    ) -> Result<Arc<RwLock<SnapshotMptDbSqlite>>>
    {
        debug!("open mpt snapshot with write {:?}", snapshot_path);
        let latest_mpt_semaphore_permit: tokio::sync::SemaphorePermit =
            executor::block_on(self.lastest_mpt_snapshot_semaphore.acquire());

        if self
            .mpt_already_open_snapshots
            .read()
            .get(&snapshot_path)
            .is_some()
        {
            bail!(ErrorKind::SnapshotAlreadyExists)
        }

        let semaphore_permit =
            executor::block_on(self.mpt_open_snapshot_semaphore.acquire());

        let snapshot_db = if create {
            SnapshotMptDbSqlite::create(
                snapshot_path.as_path(),
                &self.mpt_already_open_snapshots,
                &self.mpt_open_snapshot_semaphore,
                Some(self.lastest_mpt_snapshot_semaphore.clone()),
            )
        } else {
            let file_exists = snapshot_path.exists();
            if file_exists {
                SnapshotMptDbSqlite::open(
                    snapshot_path.as_path(),
                    /* readonly = */ false,
                    &self.mpt_already_open_snapshots,
                    &self.mpt_open_snapshot_semaphore,
                    Some(self.lastest_mpt_snapshot_semaphore.clone()),
                )
            } else {
                bail!(ErrorKind::SnapshotNotFound);
            }
        }?;

        latest_mpt_semaphore_permit.forget();
        semaphore_permit.forget();

        *self.latest_snapshot_id.write() =
            (new_snapshot_id.clone(), new_epoch_height);
        let mpt_snapshot_db = Arc::new(RwLock::new(snapshot_db));

        self.mpt_already_open_snapshots
            .write()
            .insert(snapshot_path.clone(), None);

        Ok(mpt_snapshot_db)
    }

    pub fn on_close(
        already_open_snapshots: &AlreadyOpenSnapshots<SnapshotDbSqlite>,
        open_semaphore: &Arc<Semaphore>, path: &Path, remove_on_close: bool,
    )
    {
        // Destroy at close.
        if remove_on_close {
            // When removal fails, we can not raise the error because this
            // function is called within a destructor.
            //
            // It's fine to just ignore the error because Conflux doesn't remove
            // then immediate create a snapshot, or open the snapshot for
            // modification.
            //
            // Conflux will remove orphan storage upon restart.
            Self::fs_remove_snapshot(path);
        }
        already_open_snapshots.write().remove(path);
        open_semaphore.add_permits(1);
    }

    pub fn on_close_mpt_snapshot(
        already_open_snapshots: &AlreadyOpenSnapshots<
            RwLock<SnapshotMptDbSqlite>,
        >,
        open_semaphore: &Arc<Semaphore>, path: &Path, remove_on_close: bool,
        latest_mpt_snapshot_semaphore: &Option<Arc<Semaphore>>,
    )
    {
        debug!("on_close_mpt_snapshot path {:?}", path);
        // Destroy at close.
        if remove_on_close {
            Self::fs_remove_snapshot(path);
        }
        already_open_snapshots.write().remove(path);
        open_semaphore.add_permits(1);

        if let Some(s) = latest_mpt_snapshot_semaphore {
            s.add_permits(1);
        }
    }

    fn fs_remove_snapshot(path: &Path) {
        debug!("Remove snapshot at {}", path.display());
        let path = path.to_owned();
        thread::spawn(move || {
            if let Err(e) = fs::remove_dir_all(&path) {
                error!("remove snapshot err: path={:?} err={:?}", path, e);
            }
            debug!("Finish removing snapshot at {}", path.display());
        });
    }

    fn get_merge_temp_snapshot_db_path(
        &self, old_snapshot_epoch_id: &EpochId, new_snapshot_epoch_id: &EpochId,
    ) -> PathBuf {
        self.snapshot_path.join(
            Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
                + "merge_temp_"
                + &old_snapshot_epoch_id.as_ref().to_hex::<String>()
                + &new_snapshot_epoch_id.as_ref().to_hex::<String>(),
        )
    }

    fn is_merge_temp_snapshot_db_path(&self, dir_name: &str) -> bool {
        let prefix =
            Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string() + "merge_temp_";
        dir_name.starts_with(&prefix)
    }

    fn get_full_sync_temp_snapshot_db_path(
        &self, snapshot_epoch_id: &EpochId, merkle_root: &MerkleHash,
    ) -> PathBuf {
        self.snapshot_path.join(
            Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
                + "full_sync_temp_"
                + &snapshot_epoch_id.as_ref().to_hex::<String>()
                + &merkle_root.as_ref().to_hex::<String>(),
        )
    }

    fn get_merge_temp_mpt_snapshot_db_path(
        &self, new_snapshot_epoch_id: &EpochId,
    ) -> PathBuf {
        self.mpt_snapshot_path.join(
            Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
                + "merge_temp_"
                + &new_snapshot_epoch_id.as_ref().to_hex::<String>(),
        )
    }

    fn get_latest_mpt_snapshot_db_path(&self) -> PathBuf {
        self.mpt_snapshot_path
            .join(self.get_latest_mpt_snapshot_db_name())
    }

    fn get_mpt_snapshot_db_path(&self, snapshot_epoch_id: &EpochId) -> PathBuf {
        self.mpt_snapshot_path
            .join(&self.get_snapshot_db_name(snapshot_epoch_id))
    }

    fn get_full_sync_temp_mpt_snapshot_db_path(
        &self, snapshot_epoch_id: &EpochId, merkle_root: &MerkleHash,
    ) -> PathBuf {
        self.mpt_snapshot_path.join(
            Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
                + "full_sync_temp_"
                + &snapshot_epoch_id.as_ref().to_hex::<String>()
                + &merkle_root.as_ref().to_hex::<String>(),
        )
    }

    /// Returns error when cow copy fails; Ok(true) when cow copy succeeded;
    /// Ok(false) when we are running on a system where cow copy isn't
    /// available.
    fn try_make_snapshot_cow_copy_impl(
        &self, old_snapshot_path: &Path, new_snapshot_path: &Path,
    ) -> Result<bool> {
        let mut command;
        if cfg!(target_os = "linux") {
            // XFS
            command = Command::new("cp");
            command
                .arg("-R")
                .arg("--reflink=always")
                .arg(old_snapshot_path)
                .arg(new_snapshot_path);
        } else if cfg!(target_os = "macos") {
            // APFS
            command = Command::new("cp");
            command
                .arg("-R")
                .arg("-c")
                .arg(old_snapshot_path)
                .arg(new_snapshot_path);
        } else {
            return Ok(false);
        };

        let command_result = command.output();
        if command_result.is_err() {
            fs::remove_dir_all(new_snapshot_path)?;
        }
        if !command_result?.status.success() {
            fs::remove_dir_all(new_snapshot_path)?;
            if self.force_cow {
                error!(
                    "COW copy failed, check file system support. Command {:?}",
                    command,
                );
                Err(ErrorKind::SnapshotCowCreation.into())
            } else {
                info!(
                    "COW copy failed, check file system support. Command {:?}",
                    command,
                );
                Ok(false)
            }
        } else {
            Ok(true)
        }
    }

    fn try_copy_snapshot(
        &self, old_snapshot_path: &Path, new_snapshot_path: &Path,
    ) -> Result<CopyType> {
        if self
            .try_make_snapshot_cow_copy(old_snapshot_path, new_snapshot_path)?
        {
            Ok(CopyType::Cow)
        } else {
            let mut options = CopyOptions::new();
            options.copy_inside = true; // copy recursively like `cp -r`
            fs_extra::dir::copy(old_snapshot_path, new_snapshot_path, &options)
                .map(|_| CopyType::Std)
                .map_err(|e| {
                    warn!(
                        "Fail to copy snapshot {:?}, err={:?}",
                        old_snapshot_path, e,
                    );
                    ErrorKind::SnapshotCopyFailure.into()
                })
        }
    }

    /// Returns error when cow copy fails, or when cow copy isn't supported with
    /// force_cow setting enabled; Ok(true) when cow copy succeeded;
    /// Ok(false) when cow copy isn't supported with force_cow setting disabled.
    fn try_make_snapshot_cow_copy(
        &self, old_snapshot_path: &Path, new_snapshot_path: &Path,
    ) -> Result<bool> {
        let result = self.try_make_snapshot_cow_copy_impl(
            old_snapshot_path,
            new_snapshot_path,
        );

        if result.is_err() {
            Ok(false)
        } else if result.unwrap() == false {
            if self.force_cow {
                // FIXME: Check error string.
                error!(
                    "Failed to create a new snapshot by COW. \
                     Use XFS on linux or APFS on Mac"
                );
                Err(ErrorKind::SnapshotCowCreation.into())
            } else {
                Ok(false)
            }
        } else {
            Ok(true)
        }
    }

    fn copy_and_merge(
        &self, temp_snapshot_db: &mut SnapshotDbSqlite,
        old_snapshot_epoch_id: &EpochId,
    ) -> Result<MerkleHash>
    {
        let snapshot_path = self.get_snapshot_db_path(old_snapshot_epoch_id);
        let maybe_old_snapshot_db = Self::open_snapshot_readonly(
            self,
            snapshot_path,
            /* try_open = */ false,
            old_snapshot_epoch_id,
            false,
        )?;
        let old_snapshot_db = maybe_old_snapshot_db
            .ok_or(Error::from(ErrorKind::SnapshotNotFound))?;
        temp_snapshot_db.copy_and_merge(&old_snapshot_db)
    }

    fn rename_snapshot_db<P: AsRef<Path>>(
        old_path: P, new_path: P,
    ) -> Result<()> {
        Ok(fs::rename(old_path, new_path)?)
    }

    fn defragmenting_xfs_files(&self, new_snapshot_db_path: PathBuf) {
        thread::Builder::new()
            .name("Defragmenting XFS Files".into())
            .spawn(move || {
                let paths = fs::read_dir(new_snapshot_db_path).unwrap();
                let mut files = vec![];
                for path in paths {
                    if let Ok(p) = path {
                        let f = p.path();
                        if f.is_file() {
                            files.push(f.as_path().display().to_string());
                        }
                    }
                }

                let mut command = Command::new("xfs_fsr");
                command.arg("-v");
                command.args(files);
                let command_result = command.output();
                match command_result {
                    Ok(o) => {
                        if o.status.success() {
                            info!(
                                "Defragmenting XFS files success. Command {:?}",
                                command
                            );
                        }
                    }
                    _ => {
                        error!(
                            "Defragmenting XFS files failed. Command {:?}",
                            command
                        );
                    }
                }
            })
            .unwrap();
    }

    fn copy_mpt_snapshot(&self, snapshot_epoch_id: &EpochId) -> Result<()> {
        debug!(
            "Copy mpt db for new snapshot {}, era_epoch_count {}",
            snapshot_epoch_id, self.era_epoch_count
        );
        let temp_mpt_path =
            self.get_merge_temp_mpt_snapshot_db_path(snapshot_epoch_id);
        let latest_mpt_path = self.get_latest_mpt_snapshot_db_path();
        self.try_copy_snapshot(
            latest_mpt_path.as_path(),
            temp_mpt_path.as_path(),
        )?;
        let new_mpt_snapshot_db_path =
            self.get_mpt_snapshot_db_path(snapshot_epoch_id);
        if new_mpt_snapshot_db_path.exists() {
            if let Err(e) =
                fs::remove_dir_all(&new_mpt_snapshot_db_path.as_path())
            {
                error!(
                    "remove mpt snapshot err: path={:?} err={:?}",
                    new_mpt_snapshot_db_path.as_path(),
                    e
                );
            }
        }
        Self::rename_snapshot_db(&temp_mpt_path, &new_mpt_snapshot_db_path)?;
        Ok(())
    }

    fn is_mpt_table_in_current_db_for_epoch(&self, epoch_height: u64) -> bool {
        if self.use_isolated_db_for_mpt_table {
            match self.use_isolated_db_for_mpt_table_height {
                Some(v) => epoch_height < v,
                _ => false,
            }
        } else {
            true
        }
    }
}

impl SnapshotDbManagerTrait for SnapshotDbManagerSqlite {
    type SnapshotDb = SnapshotDbSqlite;

    fn get_snapshot_dir(&self) -> &Path { self.snapshot_path.as_path() }

    fn get_mpt_snapshot_dir(&self) -> &Path { self.mpt_snapshot_path.as_path() }

    fn get_latest_mpt_snapshot_db_name(&self) -> String {
        Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
            + Self::LATEST_MPT_SNAPSHOT_DIR
    }

    fn get_snapshot_db_name(&self, snapshot_epoch_id: &EpochId) -> String {
        Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.to_string()
            + &snapshot_epoch_id.as_ref().to_hex::<String>()
    }

    fn get_snapshot_db_path(&self, snapshot_epoch_id: &EpochId) -> PathBuf {
        self.snapshot_path
            .join(&self.get_snapshot_db_name(snapshot_epoch_id))
    }

    fn get_epoch_id_from_snapshot_db_name(
        &self, snapshot_db_name: &str,
    ) -> Result<EpochId> {
        let prefix_len = Self::SNAPSHOT_DB_SQLITE_DIR_PREFIX.len();
        Ok(EpochId::from_str(&snapshot_db_name[prefix_len..])
            .map_err(|_op| "not correct snapshot db name")?)
    }

    fn new_snapshot_by_merging<'m>(
        &self, old_snapshot_epoch_id: &EpochId, snapshot_epoch_id: EpochId,
        delta_mpt: DeltaMptIterator,
        mut in_progress_snapshot_info: SnapshotInfo,
        snapshot_info_map_rwlock: &'m RwLock<PersistedSnapshotInfoMap>,
        new_epoch_height: u64,
    ) -> Result<(RwLockWriteGuard<'m, PersistedSnapshotInfoMap>, SnapshotInfo)>
    {
        debug!(
            "new_snapshot_by_merging: old={:?} new={:?} new epoch height={}",
            old_snapshot_epoch_id, snapshot_epoch_id, new_epoch_height,
        );
        // FIXME: clean-up when error happens.
        let temp_db_path = self.get_merge_temp_snapshot_db_path(
            old_snapshot_epoch_id,
            &snapshot_epoch_id,
        );

        let mut snapshot_db;
        let mut cow = false;

        let mpt_table_in_current_db =
            self.is_mpt_table_in_current_db_for_epoch(new_epoch_height);
        let new_snapshot_root = if *old_snapshot_epoch_id == NULL_EPOCH {
            let snapshot_mpt_db = if mpt_table_in_current_db {
                None
            } else {
                Some(self.open_mpt_snapshot_write(
                    self.get_mpt_snapshot_db_path(&snapshot_epoch_id),
                    true,
                    new_epoch_height,
                    &snapshot_epoch_id,
                )?)
            };

            // direct merge the first snapshot
            snapshot_db = Self::SnapshotDb::create(
                temp_db_path.as_path(),
                &self.already_open_snapshots,
                &self.open_snapshot_semaphore,
                snapshot_mpt_db,
                mpt_table_in_current_db,
            )?;
            snapshot_db.dump_delta_mpt(&delta_mpt)?;
            snapshot_db.direct_merge(None)?
        } else {
            if let Ok(copy_type) = self.try_copy_snapshot(
                self.get_snapshot_db_path(old_snapshot_epoch_id).as_path(),
                temp_db_path.as_path(),
            ) {
                cow = match copy_type {
                    CopyType::Cow => true,
                    _ => false,
                };

                // Open the copied database.
                snapshot_db = self.open_snapshot_write(
                    temp_db_path.clone(),
                    /* create = */ false,
                    new_epoch_height,
                    None,
                    &snapshot_epoch_id,
                )?;

                // Drop copied old snapshot delta mpt dump
                snapshot_db.drop_delta_mpt_dump()?;

                // iterate and insert into temp table.
                snapshot_db.dump_delta_mpt(&delta_mpt)?;

                let old_snapshot;
                let old_snapshot_db = if snapshot_db
                    .is_mpt_table_in_current_db()
                {
                    None
                } else {
                    let snapshot_path =
                        self.get_snapshot_db_path(old_snapshot_epoch_id);
                    let maybe_old_snapshot_db = Self::open_snapshot_readonly(
                        self,
                        snapshot_path,
                        /* try_open = */ false,
                        old_snapshot_epoch_id,
                        false,
                    )?;
                    old_snapshot = maybe_old_snapshot_db
                        .ok_or(Error::from(ErrorKind::SnapshotNotFound))?;
                    if old_snapshot.is_mpt_table_in_current_db() {
                        Some(&*old_snapshot)
                    } else {
                        None
                    }
                };

                snapshot_db.direct_merge(old_snapshot_db)?
            } else {
                snapshot_db = self.open_snapshot_write(
                    temp_db_path.clone(),
                    /* create = */ true,
                    new_epoch_height,
                    None,
                    &snapshot_epoch_id,
                )?;
                snapshot_db.dump_delta_mpt(&delta_mpt)?;
                self.copy_and_merge(&mut snapshot_db, old_snapshot_epoch_id)?
            }
        };

        // Create a specific MPT database for EAR checkpoint
        if !mpt_table_in_current_db
            && new_epoch_height % self.era_epoch_count == 0
        {
            self.copy_mpt_snapshot(&snapshot_epoch_id)?;
        }

        in_progress_snapshot_info.merkle_root = new_snapshot_root.clone();
        drop(snapshot_db);
        let locked = snapshot_info_map_rwlock.write();

        let new_snapshot_db_path =
            self.get_snapshot_db_path(&snapshot_epoch_id);
        Self::rename_snapshot_db(&temp_db_path, &new_snapshot_db_path)?;

        if cfg!(target_os = "linux")
            && cow
            && snapshot_epoch_id.as_fixed_bytes()[31] & 15 == 0
        {
            self.defragmenting_xfs_files(new_snapshot_db_path);
        }

        Ok((locked, in_progress_snapshot_info))
    }

    fn get_snapshot_by_epoch_id(
        &self, snapshot_epoch_id: &EpochId, try_open: bool,
        open_mpt_snapshot: bool,
    ) -> Result<Option<Arc<Self::SnapshotDb>>>
    {
        if snapshot_epoch_id.eq(&NULL_EPOCH) {
            return Ok(Some(Arc::new(Self::SnapshotDb::get_null_snapshot())));
        } else {
            let path = self.get_snapshot_db_path(snapshot_epoch_id);
            self.open_snapshot_readonly(
                path,
                try_open,
                snapshot_epoch_id,
                open_mpt_snapshot,
            )
        }
    }

    fn destroy_snapshot(&self, snapshot_epoch_id: &EpochId) -> Result<()> {
        debug!("destroy snapshot {:?}", snapshot_epoch_id);
        let path = self.get_snapshot_db_path(snapshot_epoch_id);
        let maybe_snapshot = loop {
            match self.already_open_snapshots.read().get(&path) {
                Some(Some(snapshot)) => {
                    match Weak::upgrade(snapshot) {
                        None => {
                            // This is transient and we wait for the db to be
                            // fully closed.
                            // The assumption is the same as in
                            // `open_snapshot_readonly`.
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Some(snapshot) => break Some(snapshot),
                    }
                }
                Some(None) => {
                    // This should not happen because Conflux always write on a
                    // snapshot db under a temporary name. All completed
                    // snapshots are readonly.
                    if cfg!(debug_assertions) {
                        unreachable!("Try to destroy a snapshot being open exclusively for write.")
                    } else {
                        unsafe { unreachable_unchecked() }
                    }
                }
                None => break None,
            };
        };

        match maybe_snapshot {
            None => {
                if snapshot_epoch_id.ne(&NULL_EPOCH) {
                    Self::fs_remove_snapshot(&path);
                }
            }
            Some(snapshot) => {
                snapshot.set_remove_on_last_close();
            }
        };

        {
            // destory MPT snapshot
            let mpt_snapshot_path =
                self.get_mpt_snapshot_db_path(&snapshot_epoch_id);

            let maybe_snapshot = loop {
                match self
                    .mpt_already_open_snapshots
                    .read()
                    .get(&mpt_snapshot_path)
                {
                    Some(Some(snapshot)) => match Weak::upgrade(snapshot) {
                        None => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Some(snapshot) => break Some(snapshot),
                    },
                    Some(None) => {
                        if cfg!(debug_assertions) {
                            unreachable!("Try to destroy a snapshot being open exclusively for write.")
                        } else {
                            unsafe { unreachable_unchecked() }
                        }
                    }
                    None => break None,
                };
            };

            match maybe_snapshot {
                None => {
                    if snapshot_epoch_id.ne(&NULL_EPOCH) {
                        debug!(
                            "destroy_mpt_snapshot remove mpt db {:?}",
                            mpt_snapshot_path
                        );
                        Self::fs_remove_snapshot(&mpt_snapshot_path);
                    }
                }
                Some(snapshot) => {
                    snapshot.read().set_remove_on_last_close();
                }
            };
        }

        Ok(())
    }

    fn new_temp_snapshot_for_full_sync(
        &self, snapshot_epoch_id: &EpochId, merkle_root: &MerkleHash,
        epoch_height: u64,
    ) -> Result<Self::SnapshotDb>
    {
        let mpt_table_in_current_db =
            self.is_mpt_table_in_current_db_for_epoch(epoch_height);
        let temp_mpt_snapshot_path = if mpt_table_in_current_db {
            None
        } else {
            Some(self.get_full_sync_temp_mpt_snapshot_db_path(
                snapshot_epoch_id,
                merkle_root,
            ))
        };

        let temp_db_path = self.get_full_sync_temp_snapshot_db_path(
            snapshot_epoch_id,
            merkle_root,
        );
        self.open_snapshot_write(
            temp_db_path.to_path_buf(),
            /* create = */ true,
            epoch_height,
            temp_mpt_snapshot_path,
            snapshot_epoch_id,
        )
    }

    fn finalize_full_sync_snapshot<'m>(
        &self, snapshot_epoch_id: &EpochId, merkle_root: &MerkleHash,
        snapshot_info_map_rwlock: &'m RwLock<PersistedSnapshotInfoMap>,
    ) -> Result<RwLockWriteGuard<'m, PersistedSnapshotInfoMap>>
    {
        let temp_mpt_snapshot_path = self
            .get_full_sync_temp_mpt_snapshot_db_path(
                snapshot_epoch_id,
                merkle_root,
            );
        let latest_mpt_snapshot_path = self.get_latest_mpt_snapshot_db_path();

        let temp_db_path = self.get_full_sync_temp_snapshot_db_path(
            snapshot_epoch_id,
            merkle_root,
        );
        let final_db_path = self.get_snapshot_db_path(snapshot_epoch_id);
        let locked = snapshot_info_map_rwlock.write();

        if temp_mpt_snapshot_path.exists() {
            if latest_mpt_snapshot_path.exists() {
                debug!(
                    "Remove latest mpt snapshot {:?}",
                    latest_mpt_snapshot_path
                );
                if let Err(e) =
                    fs::remove_dir_all(&latest_mpt_snapshot_path.as_path())
                {
                    error!(
                        "remove snapshot err: path={:?} err={:?}",
                        latest_mpt_snapshot_path.as_path(),
                        e
                    );
                }
            }

            Self::rename_snapshot_db(
                &temp_mpt_snapshot_path,
                &latest_mpt_snapshot_path,
            )?;
            self.copy_mpt_snapshot(snapshot_epoch_id)?;
        }

        Self::rename_snapshot_db(&temp_db_path, &final_db_path)?;
        Ok(locked)
    }

    fn recovery_lastest_mpt_snapshot(
        &self, snapshot_epoch_id: &EpochId,
    ) -> Result<()> {
        // Replace the latest MPT snapshot with the MPT snapshot of the
        // specified snapshot_epoch_id
        let latest_mpt_snapshot_path = self.get_latest_mpt_snapshot_db_path();
        if latest_mpt_snapshot_path.exists() {
            debug!("remvoe mpt snapshot {:?}", latest_mpt_snapshot_path);
            if let Err(e) =
                fs::remove_dir_all(&latest_mpt_snapshot_path.as_path())
            {
                error!(
                    "remove mpt snapshot err: path={:?} err={:?}",
                    latest_mpt_snapshot_path.as_path(),
                    e
                );
            }
        }

        let source = self.get_mpt_snapshot_db_path(snapshot_epoch_id);
        if source.exists() {
            debug!(
                "Copy mpt db for latest from snapshot {:?}  ",
                snapshot_epoch_id
            );
            let temp_mpt_path =
                self.get_merge_temp_mpt_snapshot_db_path(&snapshot_epoch_id);

            self.try_copy_snapshot(source.as_path(), temp_mpt_path.as_path())?;
            Self::rename_snapshot_db(&temp_mpt_path, &latest_mpt_snapshot_path)
        } else {
            debug!("mpt snapshot for epoch {} not exist", snapshot_epoch_id);
            // recreate latest MPT database
            SnapshotMptDbSqlite::create(
                latest_mpt_snapshot_path.as_path(),
                &Default::default(),
                &Arc::new(Semaphore::new(self.max_open_snapshots as usize)),
                None,
            )?;
            Ok(())
        }
    }

    fn is_temp_snapshot_db_path(&self, dir_name: &str) -> bool {
        self.is_merge_temp_snapshot_db_path(dir_name)
    }
}

use crate::{
    impls::{
        delta_mpt::DeltaMptIterator, errors::*,
        storage_db::snapshot_db_sqlite::*,
        storage_manager::PersistedSnapshotInfoMap,
    },
    storage_db::{SnapshotDbManagerTrait, SnapshotDbTrait, SnapshotInfo},
};
use fs_extra::dir::CopyOptions;
use futures::executor;
use parking_lot::{Mutex, RwLock, RwLockWriteGuard};
use primitives::{EpochId, MerkleHash, NULL_EPOCH};
use rustc_hex::ToHex;
use std::{
    collections::HashMap,
    fs,
    hint::unreachable_unchecked,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::{Arc, Weak},
    thread,
    time::Duration,
};
use tokio::sync::Semaphore;

use super::snapshot_mpt_db_sqlite::SnapshotMptDbSqlite;
