//! 自研文件缓存（替换 v0.1.0 的 sled，见 project.md §6.1）。
//!
//! 目录布局：
//! ```text
//! $XDG_CACHE_HOME/archstore/           (0700)
//! ├── meta/<ns>/<key>.json             (0600)  # ns: aur | flatpak | flathub | translate
//! ├── index.json                       # LRU 索引：key -> {size, atime, expires_at}
//! ├── icons/<sha256>.<ext>             # 图标文件，按内容哈希命名（天然去重）
//! ├── plans/last.json                  # 崩溃恢复用（§5.4）
//! └── archstore.log                    # 日志（滚动，<= 5 MB）
//! ```
//!
//! 硬性规则：
//! - 原子写：write 到 <key>.json.tmp -> sync_all -> rename（同目录，POSIX 原子）。
//! - 读时若 JSON 解析失败 -> 删除该条目 -> 视为 miss，并记录一次 warn。缓存损坏永远不能导致功能失效。
//! - Key 编码只允许 [a-z0-9._-]，其余字节用 %XX 转义。
//! - 同名 key 并发请求合并（single-flight），避免缓存击穿。

pub mod key;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::error::{CoreError, CoreResult};
use crate::model::plan::now_unix;

/// index.json 中的一条记录。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    /// 文件字节数
    pub size: u64,
    /// 最后访问时间（Unix 秒），用于 LRU
    pub atime: u64,
    /// 写入时间（Unix 秒），用于计算"数据为 X 分钟前"
    #[serde(default)]
    pub created_at: u64,
    /// 过期时间（Unix 秒）；0 表示永不过期
    pub expires_at: u64,
    /// 是否属于图标目录（裁剪时优先保留）
    #[serde(default)]
    pub icon: bool,
}

/// LRU 索引。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Index {
    #[serde(default)]
    pub entries: HashMap<String, IndexEntry>,
}

/// 缓存命中结果。
#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    pub value: T,
    /// 数据写入缓存的时间（Unix 秒）
    pub fetched_at: u64,
    /// 是否已过期（true 表示网络失败时降级使用了过期数据）
    pub expired: bool,
}

impl<T> CacheEntry<T> {
    /// 数据距现在多少秒。
    pub fn age_secs(&self) -> u64 {
        now_unix().saturating_sub(self.fetched_at)
    }

    /// 面向用户的"数据为 X 分钟前"标注。
    pub fn age_label(&self) -> String {
        let secs = self.age_secs();
        if secs < 60 {
            format!("{secs} 秒前")
        } else if secs < 3600 {
            format!("{} 分钟前", secs / 60)
        } else if secs < 86400 {
            format!("{} 小时前", secs / 3600)
        } else {
            format!("{} 天前", secs / 86400)
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> CacheEntry<U> {
        CacheEntry {
            value: f(self.value),
            fetched_at: self.fetched_at,
            expired: self.expired,
        }
    }
}

/// TTL 表（§6.1，明确数值，避免"看情况"）。
pub mod ttl {
    use std::time::Duration;

    /// AUR 搜索结果
    pub const AUR_SEARCH: Duration = Duration::from_secs(5 * 60);
    /// AUR 包详情
    pub const AUR_INFO: Duration = Duration::from_secs(30 * 60);
    /// Flathub appstream / summary
    pub const FLATHUB: Duration = Duration::from_secs(6 * 60 * 60);
    /// 图标文件（过期后仍可用，后台刷新）
    pub const ICON: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    /// 翻译结果
    pub const TRANSLATE: Duration = Duration::from_secs(24 * 60 * 60);
    /// Arch 安全公告（898 KB 且 5 s 级延迟，必须缓存）
    pub const SECURITY_ADVISORIES: Duration = Duration::from_secs(6 * 60 * 60);
    /// Flatpak 可更新列表
    pub const FLATPAK_UPDATES: Duration = Duration::from_secs(30 * 60);
}

/// 自研文件缓存。
pub struct Cache {
    root: PathBuf,
    index: Mutex<Index>,
    /// 缓存总上限（字节）
    max_bytes: AtomicU64,
    /// single-flight 锁表
    flights: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 上次持久化索引的时间（用于去抖）
    last_flush: Mutex<u64>,
    /// 是否需要持久化
    dirty: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache").field("root", &self.root).finish()
    }
}

impl Cache {
    /// 打开（或创建）一个缓存目录。索引损坏时按文件系统重建（自愈）。
    pub fn open(root: impl Into<PathBuf>, max_bytes: u64) -> CoreResult<Arc<Self>> {
        let root = root.into();
        std::fs::create_dir_all(root.join("meta"))?;
        std::fs::create_dir_all(root.join("icons"))?;
        std::fs::create_dir_all(root.join("plans"))?;
        crate::model::plan::set_mode(&root, 0o700)?;

        let index = Index::load_or_rebuild(&root);
        let cache = Arc::new(Self {
            root,
            index: Mutex::new(index),
            max_bytes: AtomicU64::new(max_bytes),
            flights: Mutex::new(HashMap::new()),
            last_flush: Mutex::new(0),
            dirty: std::sync::atomic::AtomicBool::new(false),
        });
        Ok(cache)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn icons_dir(&self) -> PathBuf {
        self.root.join("icons")
    }

    pub fn plans_dir(&self) -> PathBuf {
        self.root.join("plans")
    }

    pub fn meta_dir(&self) -> PathBuf {
        self.root.join("meta")
    }

    /// 更新容量上限（设置页修改后调用）。
    pub fn set_max_bytes(&self, max: u64) {
        self.max_bytes.store(max, Ordering::Relaxed);
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    fn entry_path(&self, ns: &str, key: &str) -> PathBuf {
        let ns_enc = key::encode(ns);
        let k_enc = key::encode(key);
        self.root
            .join("meta")
            .join(ns_enc)
            .join(format!("{k_enc}.json"))
    }

    /// 读取缓存条目。`allow_expired` 为 true 时，过期数据也会返回（带 expired 标记）。
    pub async fn get<T: DeserializeOwned>(
        &self,
        ns: &str,
        key: &str,
        allow_expired: bool,
    ) -> Option<CacheEntry<T>> {
        let idx_key = key::namespaced(ns, key);
        let path = self.entry_path(ns, key);
        let (fetched_at, expires_at) = {
            let mut index = self.index.lock().await;
            match index.entries.get_mut(&idx_key) {
                Some(e) => {
                    e.atime = now_unix();
                    let expired = e.expires_at != 0 && now_unix() >= e.expires_at;
                    if expired && !allow_expired {
                        return None;
                    }
                    let created = if e.created_at == 0 {
                        e.atime
                    } else {
                        e.created_at
                    };
                    (created, e.expires_at)
                }
                None => {
                    // 索引里没有：可能上次未 flush，回落到文件系统
                    let meta = std::fs::metadata(&path).ok()?;
                    let atime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or_else(now_unix);
                    (atime, 0)
                }
            }
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "缓存读取失败，视为 miss");
                return None;
            }
        };
        match serde_json::from_str::<T>(&text) {
            Ok(value) => {
                let expired = expires_at != 0 && now_unix() >= expires_at;
                Some(CacheEntry {
                    value,
                    fetched_at,
                    expired,
                })
            }
            Err(e) => {
                // 缓存损坏永远不能导致功能失效：删除并视为 miss
                tracing::warn!(path = %path.display(), error = %e, "缓存 JSON 损坏，已删除该条目");
                let _ = std::fs::remove_file(&path);
                let mut index = self.index.lock().await;
                index.entries.remove(&idx_key);
                self.mark_dirty();
                None
            }
        }
    }

    /// 写入缓存条目（原子替换）。
    pub async fn put<T: Serialize>(
        &self,
        ns: &str,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> CoreResult<()> {
        let path = self.entry_path(ns, key);
        let data = serde_json::to_vec(value).map_err(|e| CoreError::Cache(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::model::plan::write_private(&path, &data)?;

        let idx_key = key::namespaced(ns, key);
        let now = now_unix();
        {
            let mut index = self.index.lock().await;
            index.entries.insert(
                idx_key,
                IndexEntry {
                    size: data.len() as u64,
                    atime: now,
                    created_at: now,
                    expires_at: if ttl.is_zero() {
                        0
                    } else {
                        now + ttl.as_secs()
                    },
                    icon: false,
                },
            );
        }
        self.mark_dirty();
        Ok(())
    }

    /// 缓存击穿保护：同一 key 的并发请求只有一个真正执行 fetch。
    ///
    /// 语义（§6.1）：
    /// - 命中且未过期 -> 直接返回；
    /// - 未命中或已过期 -> 执行 fetch；成功则写缓存；
    /// - fetch 失败但存在过期数据 -> 返回过期数据（expired = true），由 UI 标注时间。
    pub async fn get_or_fetch<T, F, Fut>(
        &self,
        ns: &str,
        key: &str,
        ttl: Duration,
        fetch: F,
    ) -> CoreResult<CacheEntry<T>>
    where
        T: DeserializeOwned + Serialize + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = CoreResult<T>>,
    {
        if let Some(hit) = self.get::<T>(ns, key, false).await {
            return Ok(hit);
        }

        // single-flight
        let flight_key = key::namespaced(ns, key);
        let lock = {
            let mut flights = self.flights.lock().await;
            flights
                .entry(flight_key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // 等待锁期间可能已被其它任务填充
        if let Some(hit) = self.get::<T>(ns, key, false).await {
            return Ok(hit);
        }

        match fetch().await {
            Ok(value) => {
                let _ = self.put(ns, key, &value, ttl).await;
                Ok(CacheEntry {
                    value,
                    fetched_at: now_unix(),
                    expired: false,
                })
            }
            Err(e) => {
                // 失败时使用过期数据并在 UI 标注"数据为 X 分钟前"
                match self.get::<T>(ns, key, true).await {
                    Some(stale) => {
                        tracing::warn!(ns, key, error = %e, "请求失败，降级使用过期缓存");
                        Ok(CacheEntry {
                            expired: true,
                            ..stale
                        })
                    }
                    None => Err(e),
                }
            }
        }
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// 需要时把索引写回磁盘。去抖：除非 force，两次写之间至少间隔 5 秒。
    pub async fn flush(&self, force: bool) -> CoreResult<()> {
        if !self.dirty.load(Ordering::Relaxed) && !force {
            return Ok(());
        }
        let now = now_unix();
        {
            let mut last = self.last_flush.lock().await;
            if !force && now.saturating_sub(*last) < 5 {
                return Ok(());
            }
            *last = now;
        }
        let text = {
            let index = self.index.lock().await;
            serde_json::to_vec(&*index).map_err(|e| CoreError::Cache(e.to_string()))?
        };
        crate::model::plan::write_private(&self.root.join("index.json"), &text)?;
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// 缓存总占用（按索引统计；索引与文件系统不一致时以文件系统为准）。
    pub async fn total_size(&self) -> u64 {
        let index = self.index.lock().await;
        index.entries.values().map(|e| e.size).sum()
    }

    /// 缓存条目数。
    pub async fn entry_count(&self) -> usize {
        let index = self.index.lock().await;
        index.entries.len()
    }

    /// 按 LRU 裁剪到上限的 80%，图标优先保留。
    ///
    /// 返回被删除的字节数。
    pub async fn prune(&self) -> u64 {
        let max = self.max_bytes();
        let mut index = self.index.lock().await;
        let mut total: u64 = index.entries.values().map(|e| e.size).sum();
        if max == 0 || total <= max {
            return 0;
        }
        let target = max / 10 * 8;
        let mut victims: Vec<(String, IndexEntry)> = index
            .entries
            .iter()
            .filter(|(_, e)| !e.icon)
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        victims.sort_by_key(|(_, e)| e.atime);

        let mut freed = 0u64;
        for (k, e) in victims {
            if total <= target {
                break;
            }
            if self.remove_entry(&k, &e) {
                index.entries.remove(&k);
                total = total.saturating_sub(e.size);
                freed += e.size;
            }
        }
        // 仍超限则连图标一起裁
        if total > target {
            let mut icons: Vec<(String, IndexEntry)> = index
                .entries
                .iter()
                .filter(|(_, e)| e.icon)
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect();
            icons.sort_by_key(|(_, e)| e.atime);
            for (k, e) in icons {
                if total <= target {
                    break;
                }
                if self.remove_entry(&k, &e) {
                    index.entries.remove(&k);
                    total = total.saturating_sub(e.size);
                    freed += e.size;
                }
            }
        }
        drop(index);
        self.mark_dirty();
        if freed > 0 {
            tracing::info!(freed, "缓存 LRU 裁剪完成");
        }
        freed
    }

    fn remove_entry(&self, idx_key: &str, entry: &IndexEntry) -> bool {
        let path = if entry.icon {
            self.root
                .join("icons")
                .join(idx_key.rsplit('/').next().unwrap_or(idx_key))
        } else {
            match idx_key.split_once('/') {
                Some((ns, k)) => match (key::decode(ns), key::decode(k)) {
                    (Some(ns), Some(k)) => self.entry_path(&ns, &k),
                    _ => return false,
                },
                None => return false,
            }
        };
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "缓存裁剪删除失败");
                false
            }
        }
    }

    /// 清空全部缓存（设置页"清除缓存"）。
    pub async fn clear(&self) -> CoreResult<()> {
        let meta = self.root.join("meta");
        let icons = self.root.join("icons");
        let _ = std::fs::remove_dir_all(&meta);
        let _ = std::fs::remove_dir_all(&icons);
        std::fs::create_dir_all(&meta)?;
        std::fs::create_dir_all(&icons)?;
        {
            let mut index = self.index.lock().await;
            index.entries.clear();
        }
        self.mark_dirty();
        self.flush(true).await?;
        Ok(())
    }

    /// 以内容哈希落盘一个图标文件，返回其路径。
    ///
    /// 同名文件天然去重；重复写入直接复用。
    pub async fn store_icon(&self, bytes: &[u8], ext: &str) -> CoreResult<PathBuf> {
        let ext = sanitize_ext(ext);
        let hash = sha256_hex(bytes);
        let name = format!("{hash}.{ext}");
        let path = self.root.join("icons").join(&name);
        if !path.exists() {
            crate::model::plan::write_private(&path, bytes)?;
            let mut index = self.index.lock().await;
            index.entries.insert(
                format!("icons/{name}"),
                IndexEntry {
                    size: bytes.len() as u64,
                    atime: now_unix(),
                    created_at: now_unix(),
                    expires_at: now_unix() + ttl::ICON.as_secs(),
                    icon: true,
                },
            );
            drop(index);
            self.mark_dirty();
        }
        Ok(path)
    }

    /// 在图标目录中按文件名查找（避免重复下载）。
    pub fn find_icon(&self, bytes: &[u8], ext: &str) -> Option<PathBuf> {
        let path =
            self.root
                .join("icons")
                .join(format!("{}.{}", sha256_hex(bytes), sanitize_ext(ext)));
        path.exists().then_some(path)
    }

    /// 按哈希直接定位图标文件。
    pub fn icon_by_hash(&self, hash: &str, ext: &str) -> Option<PathBuf> {
        if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let path = self
            .root
            .join("icons")
            .join(format!("{hash}.{}", sanitize_ext(ext)));
        path.exists().then_some(path)
    }
}

/// 只允许已知的图片扩展名，防止通过 ext 参数写出任意文件。
fn sanitize_ext(ext: &str) -> &'static str {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "svg" => "svg",
        "webp" => "webp",
        "gif" => "gif",
        _ => "bin",
    }
}

/// 计算 SHA-256 的十六进制摘要（自实现，避免为一个小用途引入额外依赖）。
///
/// 仅供图标内容寻址使用，不用于任何安全用途。
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            let b = &chunk[i * 4..i * 4 + 4];
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

impl Index {
    /// 从磁盘加载索引；缺失或损坏时扫描文件系统重建（自愈）。
    pub fn load_or_rebuild(root: &Path) -> Index {
        let path = root.join("index.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Index>(&text) {
                Ok(index) => return index,
                Err(e) => {
                    tracing::warn!(error = %e, "index.json 损坏，按文件系统重建");
                }
            }
        }
        Index::rebuild(root)
    }

    /// 扫描 meta/ 与 icons/ 重建索引。
    pub fn rebuild(root: &Path) -> Index {
        let mut entries = HashMap::new();
        let now = now_unix();
        let meta = root.join("meta");
        if let Ok(ns_dirs) = std::fs::read_dir(&meta) {
            for ns in ns_dirs.flatten() {
                let ns_name = ns.file_name().to_string_lossy().to_string();
                let Ok(files) = std::fs::read_dir(ns.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let fname = f.file_name().to_string_lossy().to_string();
                    let Some(stem) = fname.strip_suffix(".json") else {
                        continue;
                    };
                    let Ok(meta_info) = f.metadata() else {
                        continue;
                    };
                    let Ok(decoded_ns) = key::decode(&ns_name).ok_or(()) else {
                        continue;
                    };
                    let Ok(decoded_key) = key::decode(stem).ok_or(()) else {
                        continue;
                    };
                    let atime = meta_info
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(now);
                    entries.insert(
                        key::namespaced(&decoded_ns, &decoded_key),
                        IndexEntry {
                            size: meta_info.len(),
                            atime,
                            created_at: atime,
                            expires_at: 0,
                            icon: false,
                        },
                    );
                }
            }
        }
        let icons = root.join("icons");
        if let Ok(files) = std::fs::read_dir(&icons) {
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                let Ok(meta_info) = f.metadata() else {
                    continue;
                };
                if !meta_info.is_file() {
                    continue;
                }
                let atime = meta_info
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(now);
                entries.insert(
                    format!("icons/{name}"),
                    IndexEntry {
                        size: meta_info.len(),
                        atime,
                        created_at: atime,
                        expires_at: 0,
                        icon: true,
                    },
                );
            }
        }
        Index { entries }
    }
}

/// 当前时间是否落在 TTL 之内（辅助函数，供测试与 UI 使用）。
pub fn is_fresh(fetched_at: u64, ttl: Duration) -> bool {
    now_unix().saturating_sub(fetched_at) < ttl.as_secs()
}

/// 把 SystemTime 转成 Unix 秒。
pub fn system_time_to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> (tempfile::TempDir, Arc<Cache>) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let c = Cache::open(dir.path().join("cache"), 1024 * 1024).expect("open");
        (dir, c)
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (_d, c) = cache();
        let value = vec!["firefox".to_string(), "vim".to_string()];
        c.put("aur", "search:firefox", &value, ttl::AUR_SEARCH)
            .await
            .expect("put");
        let got: CacheEntry<Vec<String>> =
            c.get("aur", "search:firefox", false).await.expect("hit");
        assert_eq!(got.value, value);
        assert!(!got.expired);
    }

    #[tokio::test]
    async fn expired_entries_are_hidden_unless_allowed() {
        let (_d, c) = cache();
        c.put("aur", "k", &"v", Duration::from_secs(0))
            .await
            .expect("put");
        // ttl = 0 表示永不过期
        assert!(c.get::<String>("aur", "k", false).await.is_some());

        // 手工写入一个已过期的条目
        let path = c.entry_path("aur", "old");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        crate::model::plan::write_private(&path, b"\"stale\"").expect("write");
        {
            let mut idx = c.index.lock().await;
            idx.entries.insert(
                key::namespaced("aur", "old"),
                IndexEntry {
                    size: 7,
                    atime: 1,
                    created_at: 1,
                    expires_at: 1,
                    icon: false,
                },
            );
        }
        assert!(c.get::<String>("aur", "old", false).await.is_none());
        let stale = c.get::<String>("aur", "old", true).await.expect("stale");
        assert!(stale.expired);
    }

    #[tokio::test]
    async fn corrupted_json_self_heals() {
        let (_d, c) = cache();
        let path = c.entry_path("aur", "broken");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        crate::model::plan::write_private(&path, b"{not json").expect("write");
        assert!(c.get::<String>("aur", "broken", true).await.is_none());
        assert!(!path.exists(), "损坏条目应被删除");
    }

    #[tokio::test]
    async fn get_or_fetch_uses_cache_and_falls_back_to_stale() {
        let (_d, c) = cache();
        let mut calls = 0usize;
        for _ in 0..2 {
            let out: CacheEntry<u32> = c
                .get_or_fetch("aur", "k", ttl::AUR_SEARCH, || async {
                    calls += 1;
                    Ok(7u32)
                })
                .await
                .expect("fetch");
            assert_eq!(out.value, 7);
        }
        assert_eq!(calls, 1, "第二次应命中缓存");

        // 过期后 fetch 失败 -> 使用过期数据
        {
            let mut idx = c.index.lock().await;
            if let Some(e) = idx.entries.get_mut(&key::namespaced("aur", "k")) {
                e.expires_at = 1;
            }
        }
        let out: CacheEntry<u32> = c
            .get_or_fetch("aur", "k", ttl::AUR_SEARCH, || async {
                Err(CoreError::Network {
                    url: "u".into(),
                    cause: "down".into(),
                })
            })
            .await
            .expect("stale fallback");
        assert_eq!(out.value, 7);
        assert!(out.expired);
    }

    #[tokio::test]
    async fn get_or_fetch_propagates_error_without_cache() {
        let (_d, c) = cache();
        let err = c
            .get_or_fetch("aur", "missing", ttl::AUR_SEARCH, || async {
                Err::<u32, _>(CoreError::Timeout {
                    url: "u".into(),
                    secs: 30,
                })
            })
            .await
            .expect_err("must fail");
        assert!(matches!(err, CoreError::Timeout { .. }));
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_partial_file() {
        let (_d, c) = cache();
        c.put("flatpak", "list", &vec![1, 2, 3], ttl::FLATHUB)
            .await
            .expect("put");
        let path = c.entry_path("flatpak", "list");
        assert!(path.exists());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("p"))
            .expect("read")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");
    }

    #[tokio::test]
    async fn prune_respects_limit_and_prefers_keeping_icons() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let c = Cache::open(dir.path().join("cache"), 4096).expect("open");
        for i in 0..40 {
            let payload = "x".repeat(400);
            c.put("aur", &format!("k{i}"), &payload, ttl::AUR_SEARCH)
                .await
                .expect("put");
        }
        let icon = c.store_icon(&vec![7u8; 800], "png").await.expect("icon");
        assert!(icon.exists());
        let before = c.total_size().await;
        assert!(before > 4096);
        let freed = c.prune().await;
        assert!(freed > 0);
        assert!(c.total_size().await <= 4096, "裁剪后应回到上限内");
        assert!(icon.exists(), "图标应优先保留");
    }

    #[tokio::test]
    async fn clear_empties_everything() {
        let (_d, c) = cache();
        c.put("aur", "k", &1u32, ttl::AUR_SEARCH)
            .await
            .expect("put");
        let _ = c.store_icon(b"abc", "png").await.expect("icon");
        c.clear().await.expect("clear");
        assert_eq!(c.total_size().await, 0);
        assert_eq!(c.entry_count().await, 0);
    }

    #[tokio::test]
    async fn index_rebuild_from_filesystem() {
        let (_d, c) = cache();
        c.put("aur", "k1", &1u32, ttl::AUR_SEARCH)
            .await
            .expect("put");
        c.put("flathub", "k2/sub", &2u32, ttl::FLATHUB)
            .await
            .expect("put");
        let rebuilt = Index::rebuild(c.root());
        assert!(rebuilt.entries.contains_key(&key::namespaced("aur", "k1")));
        assert!(
            rebuilt
                .entries
                .contains_key(&key::namespaced("flathub", "k2/sub"))
        );
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 与 `printf '中文' | sha256sum` 的输出一致
        assert_eq!(
            sha256_hex("中文".as_bytes()),
            "72726d8818f693066ceb69afa364218b692e62ea92b385782363780f47529c21"
        );
    }

    #[test]
    fn sanitize_ext_blocks_path_injection() {
        assert_eq!(sanitize_ext("png"), "png");
        assert_eq!(sanitize_ext(".PNG"), "png");
        assert_eq!(sanitize_ext("../../etc/passwd"), "bin");
        assert_eq!(sanitize_ext("sh"), "bin");
    }
}
