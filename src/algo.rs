//! 纯函数：选重后缀、码长、按键数、按键对当量。
//! 移植自原 Python 服务，逻辑一一对应。

use std::collections::HashMap;
use std::sync::Mutex;
use std::path::PathBuf;
use lazy_static::lazy_static;
use unicode_general_category::{get_general_category, GeneralCategory};

const PAGE_DOWN_KEY: char = '=';

lazy_static! {
    static ref PAIR_EQ: Mutex<Option<HashMap<String, f64>>> = Mutex::new(None);
    static ref PAIR_EQ_PATH: Mutex<PathBuf> = Mutex::new(PathBuf::from("pair_equivalence.txt"));
}

/// 透传字符：数字、空白、ASCII 字母、标点/符号，不计入字词编码查询
pub fn is_passthrough(ch: char) -> bool {
    if ch.is_whitespace() || ch.is_ascii_digit() {
        return true;
    }
    if ch.is_ascii_alphabetic() {
        return true;
    }
    // Unicode 标点 (P*) 或符号 (S*)
    let cat = get_general_category(ch);
    matches!(
        cat,
        GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::OtherPunctuation
            | GeneralCategory::MathSymbol
            | GeneralCategory::CurrencySymbol
            | GeneralCategory::ModifierSymbol
            | GeneralCategory::OtherSymbol
    )
}

/// 选重后缀：pos 从 1 开始
pub fn select_suffix(pos: usize, select_keys: &str) -> String {
    let page_size = select_keys.chars().count();
    if page_size == 0 || pos <= 1 {
        return "?".to_string();
    }
    let idx = pos - 1;
    if idx < page_size {
        return select_keys.chars().nth(idx).unwrap().to_string();
    }
    let page_count = idx / page_size;
    let pos_in_page = idx % page_size;
    let mut s = String::new();
    for _ in 0..page_count {
        s.push(PAGE_DOWN_KEY);
    }
    s.push(select_keys.chars().nth(pos_in_page).unwrap());
    s
}

/// 需要按几次键（含选重/末位确认键）
pub fn key_presses(code: &str, max_len: usize, pos: usize, select_keys: &str, candidate_count: usize) -> usize {
    if pos > 1 {
        let suffix = select_suffix(pos, select_keys);
        if code.ends_with(&suffix) {
            return code.chars().count();
        }
        return code.chars().count() + suffix.chars().count();
    }
    if code.chars().count() < max_len || candidate_count > 1 {
        return code.chars().count() + 1;
    }
    code.chars().count()
}

/// 展示用编码（含末位确认键）
pub fn code_display(code: &str, max_len: usize, pos: usize, select_keys: &str, candidate_count: usize) -> String {
    if pos > 1 {
        let suffix = select_suffix(pos, select_keys);
        if code.ends_with(&suffix) {
            return code.to_string();
        }
        return format!("{}{}", code, suffix);
    }
    if code.chars().count() < max_len || candidate_count > 1 {
        let first_key = select_keys.chars().next().unwrap_or('_');
        return format!("{}{}", code, first_key);
    }
    code.to_string()
}

// ─────────────────────────────────────────────────────────────
// 按键对当量
// ─────────────────────────────────────────────────────────────

/// 加载当量表（懒加载，仅一次）
fn ensure_pair_eq() -> std::sync::MutexGuard<'static, Option<HashMap<String, f64>>> {
    let mut guard = PAIR_EQ.lock().unwrap();
    if guard.is_none() {
        let path = PAIR_EQ_PATH.lock().unwrap().clone();
        let mut map = HashMap::new();
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 2 && parts[0].chars().count() == 2 {
                    if let Ok(v) = parts[1].parse::<f64>() {
                        map.insert(parts[0].to_string(), v);
                    }
                }
            }
        }
        *guard = Some(map);
    }
    guard
}

/// 按键序列的平均当量（相邻键对）
pub fn pair_equivalence_avg(key_seq: &str) -> Option<f64> {
    let guard = ensure_pair_eq();
    let map = match guard.as_ref() {
        Some(m) => m,
        None => return None,
    };
    if key_seq.chars().count() < 2 || map.is_empty() {
        return None;
    }
    let seq: String = key_seq.to_lowercase();
    let chars: Vec<char> = seq.chars().collect();
    let mut total = 0.0;
    let mut n = 0;
    for i in 0..chars.len().saturating_sub(1) {
        let pair: String = vec![chars[i], chars[i + 1]].iter().collect();
        if let Some(v) = map.get(&pair) {
            total += v;
            n += 1;
        }
    }
    if n > 0 { Some(total / n as f64) } else { None }
}

/// 统计按键频次（用于键盘热力图），做 NFKC 与中文标点映射
pub fn build_key_counts(key_seq: &str) -> HashMap<char, usize> {
    use unicode_normalization::UnicodeNormalization;
    let seq = key_seq.nfc().collect::<String>();
    let cjk_map: HashMap<char, char> = [
        ('。', '.'), ('、', '/'), ('—', '-'), ('…', '.'),
        ('「', '['), ('」', ']'), ('『', '['), ('』', ']'), ('【', '['), ('】', ']'),
        ('《', '<'), ('》', '>'), ('·', '`'),
    ]
    .iter()
    .cloned()
    .collect();
    let mut counts: HashMap<char, usize> = HashMap::new();
    for ch in seq.chars() {
        let c = cjk_map.get(&ch).copied().unwrap_or(ch).to_ascii_lowercase();
        let c = if c == '_' { ' ' } else { c };
        *counts.entry(c).or_insert(0) += 1;
    }
    counts
}
