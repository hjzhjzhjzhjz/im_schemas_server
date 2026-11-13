//! 码表内存缓存：把每个方案的码表与词频解析后常驻内存，避免查询时反复回查 SQLite。
//! 查询链路（DP 切分、反查、统计）只读这些 HashMap，性能从「每次 O(n²) 次 DB 往返」降到纯内存查找。

use crate::schema::SchemaInfo;
use std::collections::HashMap;

/// 单个方案的全部已解析数据，常驻内存。
#[derive(Clone)]
pub struct SchemaCache {
    pub info: SchemaInfo,
    /// 码表总行数（对应原 list 接口里的 count）
    pub entry_count: usize,
    /// 码表内最长词条的字符数。DP 切分用它作为探测上界——取自数据本身，
    /// 而不是拿「编码最大长度 max_len」去硬猜词条字数（两者毫无关系）。
    pub max_word_chars: usize,
    /// word -> [(code, pos), ...]（同一词可能多编码，按 pos 升序）
    pub word_to_codes: HashMap<String, Vec<(String, i64)>>,
    /// code -> [(word, pos), ...]（同一编码可能多字词，按 pos 升序）
    pub code_to_words: HashMap<String, Vec<(String, i64)>>,
}

impl SchemaCache {
    pub fn new(info: SchemaInfo, entry_count: usize, codes: Vec<(String, String, i64)>) -> Self {
        let mut word_to_codes: HashMap<String, Vec<(String, i64)>> = HashMap::new();
        let mut code_to_words: HashMap<String, Vec<(String, i64)>> = HashMap::new();
        let mut max_word_chars = 1usize;
        for (word, code, pos) in codes {
            max_word_chars = max_word_chars.max(word.chars().count());
            word_to_codes
                .entry(word.clone())
                .or_default()
                .push((code.clone(), pos));
            code_to_words.entry(code).or_default().push((word, pos));
        }
        SchemaCache {
            info,
            entry_count,
            max_word_chars,
            word_to_codes,
            code_to_words,
        }
    }
}

/// 切分/反查所需的查表接口。返回借用切片而非克隆 Vec：DP 内层会做 O(n·L)
/// 次查表，命中即深拷贝整个 Vec<(String,i64)> 是纯浪费——只读即可。
pub trait CodeLookup {
    /// word -> 该词的所有编码；未命中返回空切片。
    fn word_codes(&self, word: &str) -> &[(String, i64)];
    /// code -> 该编码的所有字词；未命中返回空切片。
    fn code_words(&self, code: &str) -> &[(String, i64)];
    /// 最长词条字符数，作为 DP 切分探测上界。
    fn max_word_chars(&self) -> usize;
}

impl CodeLookup for SchemaCache {
    fn word_codes(&self, word: &str) -> &[(String, i64)] {
        match self.word_to_codes.get(word) {
            Some(v) => v.as_slice(),
            None => &[],
        }
    }
    fn code_words(&self, code: &str) -> &[(String, i64)] {
        match self.code_to_words.get(code) {
            Some(v) => v.as_slice(),
            None => &[],
        }
    }
    fn max_word_chars(&self) -> usize {
        self.max_word_chars
    }
}
