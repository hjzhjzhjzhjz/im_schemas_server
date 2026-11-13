//! 类型定义与 TSV 码表/词频解析

use serde::Serialize;

/// 码表方案元信息
#[derive(Debug, Clone, Serialize)]
pub struct SchemaInfo {
    pub name: String,
    pub owner_id: String,
    pub owner_alias: String,
    pub select_keys: String,
    pub max_len: i64,
    pub punct_key: String,
    pub custom_font: String,
    pub count: i64,
}

/// 解析码表 TSV：每行 `code<TAB>word`，忽略空行与 `#` 注释。
/// 容错：无 Tab 时按空白切分取前两列。
pub fn parse_tsv(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (code, word) = if line.contains('\t') {
            let mut parts = line.split('\t');
            (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string())
        } else {
            let sp: Vec<&str> = line.split_whitespace().collect();
            if sp.len() < 2 {
                continue;
            }
            (sp[0].to_string(), sp[1].to_string())
        };
        if !code.is_empty() && !word.is_empty() {
            entries.push((code, word));
        }
    }
    entries
}

/// 解析词频 TSV：每行 `word<TAB>freq`。
pub fn parse_freq_tsv(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (word, freq) = if line.contains('\t') {
            let mut parts = line.split('\t');
            (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string())
        } else {
            let sp: Vec<&str> = line.split_whitespace().collect();
            if sp.len() < 2 {
                continue;
            }
            (sp[0].to_string(), sp[1].to_string())
        };
        if !word.is_empty() && !freq.is_empty() {
            entries.push((word, freq));
        }
    }
    entries
}
