use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, trace, warn};

const SLOT_COUNT: usize = 2;
const ATTEMPTS: usize = 4;
const WAL_HEADER_BYTES: usize = 32;
const COPY_BUFFER_BYTES: usize = 256 * 1024;
const WAL_MAGIC: u32 = 0x377f_0682;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileRevision {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: i64,
    modified_ns: i64,
}

impl FileRevision {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self::from_metadata(&metadata))
    }

    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            modified: metadata.mtime(),
            modified_ns: metadata.mtime_nsec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WalHeader {
    salt: [u8; 8],
    revision: FileRevision,
}

#[derive(Clone, Debug, Default)]
struct SlotState {
    main: Option<FileRevision>,
    wal: Option<WalHeader>,
}

struct Slot {
    directory: PathBuf,
    state: SlotState,
}

impl Slot {
    fn database(&self) -> PathBuf {
        self.directory.join("database.db")
    }

    fn wal(&self) -> PathBuf {
        self.directory.join("database.db-wal")
    }

    fn shm(&self) -> PathBuf {
        self.directory.join("database.db-shm")
    }
}

struct DatabaseState {
    active: usize,
    slots: [Slot; SLOT_COUNT],
}

impl DatabaseState {
    fn new(root: &Path, database: &str) -> Self {
        let base = root.join(format!("{:x}", md5::compute(database)));
        Self {
            active: 0,
            slots: std::array::from_fn(|index| Slot {
                directory: base.join(format!("slot-{index}")),
                state: SlotState::default(),
            }),
        }
    }
}

pub(super) struct SnapshotReader {
    key: Vec<u8>,
    source: PathBuf,
    root: PathBuf,
    databases: std::sync::Mutex<HashMap<String, DatabaseState>>,
}

impl SnapshotReader {
    pub(super) fn new(key: Vec<u8>, source: PathBuf) -> Result<Self> {
        anyhow::ensure!(key.len() == 32, "数据库密钥长度必须为 32 字节");
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "mimicwx-db-snapshots-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        cleanup_stale_roots();
        info!("📸 数据库只读快照目录: {}", root.display());
        Ok(Self {
            key,
            source,
            root,
            databases: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn open(&self, database: &str) -> Result<Connection> {
        let mut databases = self
            .databases
            .lock()
            .map_err(|_| anyhow::anyhow!("数据库快照状态锁不可用"))?;
        let state = databases
            .entry(database.to_string())
            .or_insert_with(|| DatabaseState::new(&self.root, database));
        let mut last_error = None;
        for attempt in 0..ATTEMPTS {
            let index = 1 - state.active;
            let force = attempt > 0;
            let slot = &mut state.slots[index];
            if let Err(error) = self.refresh(database, slot, force) {
                last_error = Some(error);
                continue;
            }
            match self.connect(slot) {
                Ok(connection) => {
                    state.active = index;
                    trace!("📸 {} 快照连接已打开", database);
                    return Ok(connection);
                }
                Err(error) => {
                    warn!(
                        "⚠️ {} 快照打开失败 (第 {} 次): {}",
                        database,
                        attempt + 1,
                        error
                    );
                    slot.state = SlotState::default();
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("数据库快照打开失败: {database}")))
    }

    fn refresh(&self, database: &str, slot: &mut Slot, force: bool) -> Result<()> {
        fs::create_dir_all(&slot.directory)?;
        fs::set_permissions(&slot.directory, fs::Permissions::from_mode(0o700))?;
        if force {
            slot.state = SlotState::default();
            let _ = fs::remove_file(slot.database());
            let _ = fs::remove_file(slot.wal());
            let _ = fs::remove_file(slot.shm());
        }

        let source = self.source.join(database);
        let source_wal = PathBuf::from(format!("{}-wal", source.display()));
        let main_before = FileRevision::read(&source)?;
        let wal_before = read_wal_header(&source_wal)?;

        let main_reused =
            !force && slot.state.main == Some(main_before) && slot.database().exists();
        if !main_reused {
            copy_file(&source, &slot.database())?;
        }
        slot.state.main = Some(main_before);

        if let Some(header) = wal_before {
            let can_reuse = !force
                && slot.state.wal == Some(header)
                && slot.wal().exists()
                && fs::metadata(slot.wal())?.len() == header.revision.bytes;
            if !can_reuse {
                copy_file_range(&source_wal, &slot.wal(), 0, header.revision.bytes)?;
            }
            slot.state.wal = Some(header);
        } else {
            let _ = fs::remove_file(slot.wal());
            slot.state.wal = None;
        }
        let _ = fs::remove_file(slot.shm());

        let main_after = FileRevision::read(&source)?;
        let wal_after = read_wal_header(&source_wal)?;
        anyhow::ensure!(
            main_before == main_after,
            "数据库 {} 在主库复制期间发生变化",
            database
        );
        anyhow::ensure!(
            wal_before == wal_after,
            "数据库 {} 的 WAL 在复制期间发生变化",
            database
        );
        Ok(())
    }

    fn connect(&self, slot: &Slot) -> Result<Connection> {
        let path = slot.database();
        anyhow::ensure!(path.exists(), "快照不存在: {}", path.display());
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("打开数据库快照失败: {}", path.display()))?;
        connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        let result = unsafe {
            super::sqlite3_key(
                connection.handle() as *mut std::ffi::c_void,
                self.key.as_ptr(),
                self.key.len() as std::ffi::c_int,
            )
        };
        anyhow::ensure!(result == 0, "sqlite3_key() 失败, rc={result}");
        connection.execute_batch("PRAGMA cipher_compatibility = 4;")?;
        connection.execute_batch("PRAGMA query_only = ON;")?;
        let tables: i32 = connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .with_context(|| format!("数据库快照解密验证失败: {}", path.display()))?;
        trace!("🔓 快照 {} 解密成功, {} 个表", path.display(), tables);
        Ok(connection)
    }
}

impl Drop for SnapshotReader {
    fn drop(&mut self) {
        if let Ok(mut databases) = self.databases.lock() {
            databases.clear();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn read_wal_header(path: &Path) -> Result<Option<WalHeader>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() == 0 {
        return Ok(None);
    }
    if metadata.len() < WAL_HEADER_BYTES as u64 {
        bail!("WAL 文件头不完整: {}", path.display());
    }
    let mut file = File::open(path)?;
    let mut header = [0u8; WAL_HEADER_BYTES];
    file.read_exact(&mut header)?;
    let magic = u32::from_be_bytes(header[0..4].try_into().unwrap());
    anyhow::ensure!(magic == WAL_MAGIC, "无效的 WAL 文件头: {}", path.display());
    let mut salt = [0u8; 8];
    salt.copy_from_slice(&header[16..24]);
    Ok(Some(WalHeader {
        salt,
        revision: FileRevision::from_metadata(&metadata),
    }))
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let temporary = temporary_path(destination);
    fs::copy(source, &temporary)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, destination)?;
    Ok(())
}

fn copy_file_range(source: &Path, destination: &Path, start: u64, end: u64) -> Result<()> {
    anyhow::ensure!(start <= end, "无效的复制范围");
    let temporary = temporary_path(destination);
    let mut input = File::open(source)?;
    input.seek(SeekFrom::Start(start))?;
    let mut output = File::create(&temporary)?;
    copy_exact(&mut input, &mut output, end - start)?;
    output.flush()?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, destination)?;
    Ok(())
}

fn copy_exact(input: &mut File, output: &mut File, mut remaining: u64) -> Result<()> {
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let read = input.read(&mut buffer[..limit])?;
        if read == 0 {
            bail!("源文件在复制期间被截断");
        }
        output.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    Ok(())
}

fn temporary_path(destination: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    destination.with_file_name(format!(
        ".{}.tmp-{}-{sequence}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("snapshot"),
        std::process::id()
    ))
}

fn cleanup_stale_roots() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix("mimicwx-db-snapshots-") else {
            continue;
        };
        let Some(pid) = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}
