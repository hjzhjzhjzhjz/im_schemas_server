//! 渲染结果缓存：把已生成的 PNG 字节常驻内存，避免对同一查询反复做
//! CPU 密集的 cosmic-text 排版与逐像素绘制。
//!
//! 轻量 LRU：HashMap + 单调递增逻辑时钟，达到容量上限时淘汰最旧的一批；
//! 值用 `bytes::Bytes`（引用计数）以便命中时零拷贝取出并直接作为响应体。容量 0 表示禁用。
//!
//! 代际号（generation）：`clear()` 递增代际；在 `spawn_blocking` 里渲染的在途请求
//! 完成后，只当代际未变才回填，避免「上传新码表 → 清空缓存 → 旧渲染写回旧图」的污染。
//! 代际检查+写入 与 代际递增+清空 都在同一把锁内，保证原子。

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

struct Entry {
    png: Bytes,
    used: u64,
}

struct Inner {
    map: HashMap<String, Entry>,
    clock: u64,
    generation: u64,
}

pub struct RenderCache {
    inner: Mutex<Inner>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    /// 因代际过期而被拒绝回填的次数（在途旧渲染），可观测。
    stale_rejected: AtomicU64,
}

impl RenderCache {
    pub fn new(capacity: usize) -> Self {
        RenderCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                clock: 0,
                generation: 0,
            }),
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            stale_rejected: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.capacity > 0
    }

    /// 当前代际。渲染任务发起前读取，完成后交给 `insert_if_current` 校验。
    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// 命中则刷新「最近使用」并返回 PNG，未命中返回 None。
    pub fn get(&self, key: &str) -> Option<Bytes> {
        let mut g = self.inner.lock().unwrap();
        g.clock = g.clock.wrapping_add(1);
        let tick = g.clock;
        match g.map.get_mut(key) {
            Some(e) => {
                e.used = tick;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(e.png.clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// 回填缓存：仅当代际与发起渲染时一致才写入，否则丢弃（旧版本结果不污染缓存）。
    /// 无论如何都返回本次渲染字节，供当前请求的响应使用。
    pub fn insert_if_current(&self, key: String, png: Vec<u8>, gen: u64) -> Bytes {
        let arc = Bytes::from(png);
        if !self.enabled() {
            return arc;
        }
        let mut g = self.inner.lock().unwrap();
        if g.generation != gen {
            // 渲染期间发生了上传/清空：本次图基于旧码表，绝不写回
            self.stale_rejected.fetch_add(1, Ordering::Relaxed);
            return arc;
        }
        // 容量满时淘汰最旧的 1/4，避免每次插入都全表扫描
        if g.map.len() >= self.capacity {
            let n = (self.capacity / 4).max(1);
            let mut oldest: Vec<(u64, String)> = g
                .map
                .iter()
                .map(|(k, e)| (e.used, k.clone()))
                .collect();
            oldest.sort_unstable_by_key(|x| x.0);
            for (_, k) in oldest.into_iter().take(n) {
                g.map.remove(&k);
            }
            self.evictions.fetch_add(n as u64, Ordering::Relaxed);
        }
        g.clock = g.clock.wrapping_add(1);
        let tick = g.clock;
        g.map.insert(key, Entry {
            png: arc.clone(),
            used: tick,
        });
        arc
    }

    /// 清空条目并递增代际（上传新码表/词频、手动清空后调用）。
    /// 代际递增与清空必须在同一临界区内，否则在途渲染可能夹在两步之间写回旧图。
    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.generation = g.generation.wrapping_add(1);
        g.map.clear();
    }

    pub fn stats(&self) -> serde_json::Value {
        let g = self.inner.lock().unwrap();
        let bytes: usize = g.map.values().map(|e| e.png.len()).sum();
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        serde_json::json!({
            "enabled": self.enabled(),
            "capacity": self.capacity,
            "entries": g.map.len(),
            "bytes": bytes,
            "hits": hits,
            "misses": misses,
            "evictions": self.evictions.load(Ordering::Relaxed),
            "generation": g.generation,
            "stale_rejected": self.stale_rejected.load(Ordering::Relaxed),
            "hit_rate": if total > 0 { (hits as f64 / total as f64 * 10000.0).round() / 100.0 } else { 0.0 },
        })
    }
}
