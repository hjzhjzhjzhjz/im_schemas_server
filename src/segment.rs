//! 切分（DP 最少按键）、选重/码长计费聚合、all 组排序。

use crate::algo::*;
use crate::cache::{CodeLookup, SchemaCache};
use crate::schema::SchemaInfo;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
pub struct CodeEntry {
    pub code: String,
    pub pos: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub text: String,
    pub is_word: bool,
    pub is_single: bool,
    pub missing: bool,
    pub codes: Vec<CodeEntry>,
    pub best_code: Option<String>,
    pub best_pos: i64,
    pub key_presses: usize,
    pub code_display: String,
    pub is_select: bool,
    pub is_custom: bool,
    pub pair_eq: Option<f64>,
    pub color: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub total_keys: usize,
    pub phrase_count: usize,
    pub select_count: usize,
    pub equivalent: Option<f64>,
    pub missing_count: usize,
    pub key_counts: HashMap<char, usize>,
}

impl Stats {
    /// 排序键：越小越好（码长、选重、当量、缺字）
    pub fn sort_key(&self) -> (usize, usize, f64, usize) {
        let eq = self.equivalent.unwrap_or(9.9);
        (self.total_keys, self.select_count, eq, self.missing_count)
    }
}

/// 计算单个子串的代价（按键数）与是否缺字。
/// - 全透传字符（数字/字母/标点）：每个字符 1 键
/// - 词库命中：取最优编码的 key_presses
/// - 单字未命中：缺字（1 键，标记 missing）
/// - 多字未命中：返回 None（交由 DP 进一步切分）
fn segment_cost(lookup: &dyn CodeLookup, schema: &SchemaInfo, w: &str, force_single: bool) -> Option<(usize, bool)> {
    let chs: Vec<char> = w.chars().collect();
    if chs.iter().all(|c| is_passthrough(*c)) {
        return Some((chs.len(), false));
    }
    if force_single && chs.len() != 1 {
        return None;
    }
    let rows = lookup.word_codes(w);
    if !rows.is_empty() {
        let best = rows
            .iter()
            .min_by(|a, b| {
                a.0.chars()
                    .count()
                    .cmp(&b.0.chars().count())
                    .then(a.1.cmp(&b.1))
            })
            .unwrap();
        let kp = key_presses(
            &best.0,
            schema.max_len as usize,
            best.1 as usize,
            &schema.select_keys,
            rows.len(),
        );
        return Some((kp, false));
    }
    if chs.len() == 1 {
        return Some((1, true));
    }
    None
}

/// 为单个子串构造完整 Segment（含配色、当量、键频贡献在 stats 中聚合）。
fn build_segment(lookup: &dyn CodeLookup, schema: &SchemaInfo, w: &str, _force_single: bool) -> Segment {
    let chs: Vec<char> = w.chars().collect();
    let all_pass = chs.iter().all(|c| is_passthrough(*c));
    if all_pass {
        return Segment {
            text: w.to_string(),
            is_word: false,
            is_single: chs.len() == 1,
            missing: false,
            codes: w.chars().map(|c| CodeEntry { code: c.to_string(), pos: 1 }).collect(),
            best_code: Some(w.to_string()),
            best_pos: 1,
            key_presses: chs.len(),
            code_display: w.to_string(),
            is_select: false,
            is_custom: false,
            pair_eq: None,
            color: "#888888".to_string(),
        };
    }
    let rows = lookup.word_codes(w);
    if !rows.is_empty() {
        let best = rows
                .iter()
                .min_by(|a, b| {
                    a.0.chars()
                        .count()
                        .cmp(&b.0.chars().count())
                        .then(a.1.cmp(&b.1))
                })
                .unwrap();
            let pos = best.1 as usize;
            let cand = rows.len();
            let kp = key_presses(&best.0, schema.max_len as usize, pos, &schema.select_keys, cand);
            let disp = code_display(&best.0, schema.max_len as usize, pos, &schema.select_keys, cand);
            let is_select = pos > 1;
            let clen = best.0.chars().count();
            let max_len = schema.max_len as usize;
            let (color, is_custom) = if is_select {
                ("#e53935".to_string(), false)
            } else if clen > max_len {
                ("#1565c0".to_string(), true)
            } else if clen == max_len {
                ("#2e7d32".to_string(), false)
            } else {
                ("#ef6c00".to_string(), false)
            };
            let pair = pair_equivalence_avg(&best.0);
            return Segment {
                text: w.to_string(),
                is_word: true,
                is_single: chs.len() == 1,
                missing: false,
                codes: rows
                    .iter()
                    .map(|r| CodeEntry { code: r.0.clone(), pos: r.1 })
                    .collect(),
                best_code: Some(best.0.clone()),
                best_pos: best.1,
                key_presses: kp,
                code_display: disp,
                is_select,
                is_custom,
                pair_eq: pair,
                color,
            };
    }
    // 缺字
    Segment {
        text: w.to_string(),
        is_word: true,
        is_single: chs.len() == 1,
        missing: true,
        codes: vec![],
        best_code: None,
        best_pos: 1,
        key_presses: 1,
        code_display: "?".repeat(chs.len()),
        is_select: false,
        is_custom: false,
        pair_eq: None,
        color: "#8e24aa".to_string(),
    }
}

/// DP 最少按键切分。
pub fn query_segments(lookup: &dyn CodeLookup, schema: &SchemaInfo, text: &str, force_single: bool) -> Vec<Segment> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return vec![];
    }
    const INF: f64 = 1e9;
    let mut cost = vec![INF; n + 1];
    let mut parent = vec![0usize; n + 1];
    cost[0] = 0.0;
    // 探测上界取自码表真实最长词条；下限 8 保持连续透传字符（数字/字母/标点）
    // 的合并行为与原实现一致，上限 64 防御异常码表。
    let max_l = if force_single {
        1
    } else {
        lookup.max_word_chars().clamp(8, 64)
    };
    // 复用一个缓冲构造子串，避免 O(n·max_l) 次临时 String 分配。
    let mut wbuf = String::with_capacity(max_l * 4);
    for i in 0..n {
        if cost[i] >= INF {
            continue;
        }
        for l in 1..=max_l {
            let j = i + l;
            if j > n {
                break;
            }
            wbuf.clear();
            wbuf.extend(chars[i..j].iter().copied());
            if let Some((seg_cost, _)) = segment_cost(lookup, schema, &wbuf, force_single) {
                let cand = cost[i] + seg_cost as f64 + 0.001; // 同按键数时偏好更少分段
                if cand < cost[j] - 1e-9 {
                    cost[j] = cand;
                    parent[j] = i;
                }
            }
        }
    }
    let mut segs = Vec::new();
    let mut j = n;
    while j > 0 {
        let i = parent[j];
        let w: String = chars[i..j].iter().collect();
        segs.push(build_segment(lookup, schema, &w, force_single));
        j = i;
    }
    segs.reverse();
    segs
}

/// 聚合统计。
pub fn compute_stats(segments: &[Segment]) -> Stats {
    let mut total_keys = 0usize;
    let mut phrase = 0usize;
    let mut select = 0usize;
    let mut missing = 0usize;
    let mut eq_sum = 0.0;
    let mut eq_n = 0usize;
    let mut key_counts: HashMap<char, usize> = HashMap::new();
    for s in segments {
        total_keys += s.key_presses;
        if s.is_word && !s.missing {
            phrase += 1;
        }
        if s.is_select {
            select += 1;
        }
        if s.missing {
            missing += 1;
        }
        if let Some(eq) = s.pair_eq {
            eq_sum += eq;
            eq_n += 1;
        }
        if let Some(c) = &s.best_code {
            for (k, v) in build_key_counts(c) {
                *key_counts.entry(k).or_insert(0) += v;
            }
        }
    }
    let equivalent = if eq_n > 0 { Some(eq_sum / eq_n as f64) } else { None };
    Stats {
        total_keys,
        phrase_count: phrase,
        select_count: select,
        equivalent,
        missing_count: missing,
        key_counts,
    }
}

/// 多方案 all 对比：返回每个方案的切分与统计，按 sort_key 升序。
#[derive(Debug, Clone, Serialize)]
pub struct AllResult {
    pub schema: String,
    pub segments: Vec<Segment>,
    pub stats: Stats,
}

pub fn query_all(caches: &[Arc<SchemaCache>], text: &str) -> Vec<AllResult> {
    let mut results: Vec<AllResult> = Vec::new();
    for c in caches {
        let s: &SchemaCache = c;
        let segs = query_segments(s, &s.info, text, false);
        let stats = compute_stats(&segs);
        results.push(AllResult {
            schema: s.info.name.clone(),
            segments: segs,
            stats,
        });
    }
    results.sort_by(|a, b| {
        let ka = a.stats.sort_key();
        let kb = b.stats.sort_key();
        ka.0.cmp(&kb.0)
            .then(ka.1.cmp(&kb.1))
            .then(ka.2.partial_cmp(&kb.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(ka.3.cmp(&kb.3))
    });
    results
}
