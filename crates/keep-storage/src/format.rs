//! Binary format definitions for WAL and Snapshot files
//!
//! ## WAL File Format
//!
//! The WAL is the core `storage-durable` framed log; this module defines only the payload.
//!
//! ```text
//! Frame:  [Seq:8 LE | Len:4 LE | CRC32:4 LE | Payload:N = bincode(WalOp)]
//! ```
//!
//! ## Snapshot File Format
//!
//! ```text
//! Header: [Magic:8 | Version:4 | Created:8 | NumShards:4 | TotalEntries:8 | WalPos:8 | SHA256:32]
//! Data:   [Shard0] [Shard1] ... [ShardN]
//! Shard:  [ShardID:4 | EntryCount:4 | Entries using bincode]
//! ```

use crate::error::{PersistenceError, Result};
use keep_shared_kernel::types::KvValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::time::Duration;

/// Magic of the retired KVWAL001 WAL format; used only to refuse such files.
pub const WAL_MAGIC: &[u8; 8] = b"KVWAL001";

/// Snapshot file magic number: "KVSNAP01"
pub const SNAPSHOT_MAGIC: &[u8; 8] = b"KVSNAP01";

/// Snapshot format version
pub const SNAPSHOT_VERSION: u32 = 1;

/// WAL operation types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum WalOpType {
    Set = 1,
    Delete = 2,
    Incr = 3,
    Decr = 4,
    MSet = 5,
    MDel = 6,
    SetNx = 7,
    Lock = 8,
    Unlock = 9,
    ExtendLock = 10,
    // Collection + expiry + cas ops — added to close the durability gap (these
    // were not WAL-logged before, so they were lost on recovery).
    Cas = 11,
    Expire = 12,
    PExpire = 13,
    Persist = 14,
    HSet = 15,
    HDel = 16,
    HIncrBy = 17,
    HIncrByFloat = 18,
    SAdd = 19,
    SRem = 20,
    ZAdd = 21,
    ZRem = 22,
    ZIncrBy = 23,
    LPush = 24,
    RPush = 25,
    LPop = 26,
    RPop = 27,
    GetEx = 28,
}

impl WalOpType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(WalOpType::Set),
            2 => Some(WalOpType::Delete),
            3 => Some(WalOpType::Incr),
            4 => Some(WalOpType::Decr),
            5 => Some(WalOpType::MSet),
            6 => Some(WalOpType::MDel),
            7 => Some(WalOpType::SetNx),
            8 => Some(WalOpType::Lock),
            9 => Some(WalOpType::Unlock),
            10 => Some(WalOpType::ExtendLock),
            11 => Some(WalOpType::Cas),
            12 => Some(WalOpType::Expire),
            13 => Some(WalOpType::PExpire),
            14 => Some(WalOpType::Persist),
            15 => Some(WalOpType::HSet),
            16 => Some(WalOpType::HDel),
            17 => Some(WalOpType::HIncrBy),
            18 => Some(WalOpType::HIncrByFloat),
            19 => Some(WalOpType::SAdd),
            20 => Some(WalOpType::SRem),
            21 => Some(WalOpType::ZAdd),
            22 => Some(WalOpType::ZRem),
            23 => Some(WalOpType::ZIncrBy),
            24 => Some(WalOpType::LPush),
            25 => Some(WalOpType::RPush),
            26 => Some(WalOpType::LPop),
            27 => Some(WalOpType::RPop),
            28 => Some(WalOpType::GetEx),
            _ => None,
        }
    }
}

/// WAL operation data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalOp {
    Set {
        key: String,
        value: KvValue,
        ttl: Option<Duration>,
    },
    Delete {
        key: String,
    },
    Incr {
        key: String,
        delta: i64,
    },
    Decr {
        key: String,
        delta: i64,
    },
    MSet {
        pairs: Vec<(String, KvValue)>,
        ttl: Option<Duration>,
    },
    MDel {
        keys: Vec<String>,
    },
    SetNx {
        key: String,
        value: KvValue,
        ttl: Option<Duration>,
    },
    Lock {
        key: String,
        owner: String,
        ttl: Duration,
    },
    Unlock {
        key: String,
        owner: String,
    },
    ExtendLock {
        key: String,
        owner: String,
        ttl: Duration,
    },
    // --- appended (keep existing variant order for bincode compatibility) ---
    Cas {
        key: String,
        expected: KvValue,
        new: KvValue,
        ttl: Option<Duration>,
    },
    Expire {
        key: String,
        seconds: i64,
    },
    PExpire {
        key: String,
        milliseconds: i64,
    },
    Persist {
        key: String,
    },
    HSet {
        key: String,
        fields: Vec<(String, KvValue)>,
    },
    HDel {
        key: String,
        fields: Vec<String>,
    },
    HIncrBy {
        key: String,
        field: String,
        delta: i64,
    },
    HIncrByFloat {
        key: String,
        field: String,
        delta: f64,
    },
    SAdd {
        key: String,
        members: Vec<String>,
    },
    SRem {
        key: String,
        members: Vec<String>,
    },
    ZAdd {
        key: String,
        members: Vec<(String, f64)>,
    },
    ZRem {
        key: String,
        members: Vec<String>,
    },
    ZIncrBy {
        key: String,
        member: String,
        delta: f64,
    },
    LPush {
        key: String,
        values: Vec<KvValue>,
    },
    RPush {
        key: String,
        values: Vec<KvValue>,
    },
    LPop {
        key: String,
    },
    RPop {
        key: String,
    },
    GetEx {
        key: String,
        ttl_ms: Option<u64>,
        persist: bool,
    },
}

impl WalOp {
    pub fn op_type(&self) -> WalOpType {
        match self {
            WalOp::Set { .. } => WalOpType::Set,
            WalOp::Delete { .. } => WalOpType::Delete,
            WalOp::Incr { .. } => WalOpType::Incr,
            WalOp::Decr { .. } => WalOpType::Decr,
            WalOp::MSet { .. } => WalOpType::MSet,
            WalOp::MDel { .. } => WalOpType::MDel,
            WalOp::SetNx { .. } => WalOpType::SetNx,
            WalOp::Lock { .. } => WalOpType::Lock,
            WalOp::Unlock { .. } => WalOpType::Unlock,
            WalOp::ExtendLock { .. } => WalOpType::ExtendLock,
            WalOp::Cas { .. } => WalOpType::Cas,
            WalOp::Expire { .. } => WalOpType::Expire,
            WalOp::PExpire { .. } => WalOpType::PExpire,
            WalOp::Persist { .. } => WalOpType::Persist,
            WalOp::HSet { .. } => WalOpType::HSet,
            WalOp::HDel { .. } => WalOpType::HDel,
            WalOp::HIncrBy { .. } => WalOpType::HIncrBy,
            WalOp::HIncrByFloat { .. } => WalOpType::HIncrByFloat,
            WalOp::SAdd { .. } => WalOpType::SAdd,
            WalOp::SRem { .. } => WalOpType::SRem,
            WalOp::ZAdd { .. } => WalOpType::ZAdd,
            WalOp::ZRem { .. } => WalOpType::ZRem,
            WalOp::ZIncrBy { .. } => WalOpType::ZIncrBy,
            WalOp::LPush { .. } => WalOpType::LPush,
            WalOp::RPush { .. } => WalOpType::RPush,
            WalOp::LPop { .. } => WalOpType::LPop,
            WalOp::RPop { .. } => WalOpType::RPop,
            WalOp::GetEx { .. } => WalOpType::GetEx,
        }
    }
}

/// WAL entry: the operation and its sequence number
#[derive(Debug, Clone)]
pub struct WalEntry {
    /// Position of the entry in the WAL
    pub seq: u64,

    /// The operation
    pub op: WalOp,
}

/// Snapshot file header (72 bytes)
#[derive(Debug, Clone)]
pub struct SnapshotHeader {
    pub magic: [u8; 8],
    pub version: u32,
    pub created_at: i64,
    pub num_shards: u32,
    pub total_entries: u64,
    pub wal_position: u64,  // Position in WAL when snapshot was taken
    pub checksum: [u8; 32], // SHA256 of data
}

impl SnapshotHeader {
    pub fn new(num_shards: u32, total_entries: u64, wal_position: u64, checksum: [u8; 32]) -> Self {
        Self {
            magic: *SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            num_shards,
            total_entries,
            wal_position,
            checksum,
        }
    }

    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.magic)?;
        writer.write_all(&self.version.to_be_bytes())?;
        writer.write_all(&self.created_at.to_be_bytes())?;
        writer.write_all(&self.num_shards.to_be_bytes())?;
        writer.write_all(&self.total_entries.to_be_bytes())?;
        writer.write_all(&self.wal_position.to_be_bytes())?;
        writer.write_all(&self.checksum)?;
        Ok(())
    }

    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;

        if &magic != SNAPSHOT_MAGIC {
            return Err(PersistenceError::InvalidMagic {
                expected: SNAPSHOT_MAGIC.to_vec(),
                actual: magic.to_vec(),
            });
        }

        let mut version_bytes = [0u8; 4];
        reader.read_exact(&mut version_bytes)?;
        let version = u32::from_be_bytes(version_bytes);

        if version != SNAPSHOT_VERSION {
            return Err(PersistenceError::UnsupportedVersion(version));
        }

        let mut created_bytes = [0u8; 8];
        reader.read_exact(&mut created_bytes)?;
        let created_at = i64::from_be_bytes(created_bytes);

        let mut num_shards_bytes = [0u8; 4];
        reader.read_exact(&mut num_shards_bytes)?;
        let num_shards = u32::from_be_bytes(num_shards_bytes);

        let mut total_entries_bytes = [0u8; 8];
        reader.read_exact(&mut total_entries_bytes)?;
        let total_entries = u64::from_be_bytes(total_entries_bytes);

        let mut wal_position_bytes = [0u8; 8];
        reader.read_exact(&mut wal_position_bytes)?;
        let wal_position = u64::from_be_bytes(wal_position_bytes);

        let mut checksum = [0u8; 32];
        reader.read_exact(&mut checksum)?;

        Ok(Self {
            magic,
            version,
            created_at,
            num_shards,
            total_entries,
            wal_position,
            checksum,
        })
    }
}

/// Calculate SHA256 checksum
pub fn calculate_sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_calculation() {
        let data = b"Hello, World!";
        let hash1 = calculate_sha256(data);
        let hash2 = calculate_sha256(data);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 32);
    }
}
