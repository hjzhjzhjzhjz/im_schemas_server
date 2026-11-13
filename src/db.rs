//! SQLite 数据层：方案、码表、词频的存储与查询。
//! 全局用 `Mutex<Db>` 包裹，供 axum 多线程请求共享。

use crate::schema::{parse_freq_tsv, parse_tsv, SchemaInfo};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Db {
    conn: Connection,
}

/// 把一组 (code, word) 按 code 分组，组内按顺序赋 pos（选重位序）。
fn assign_positions(entries: &[(String, String)]) -> Vec<(String, String, i64)> {
    use std::collections::BTreeMap;
    let mut by_code: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (code, word) in entries {
        by_code.entry(code.clone()).or_default().push(word.clone());
    }
    let mut out = Vec::new();
    for (code, words) in by_code {
        for (i, word) in words.iter().enumerate() {
            out.push((code.clone(), word.clone(), (i + 1) as i64));
        }
    }
    out
}

impl Db {
    pub fn open(data_dir: &Path) -> rusqlite::Result<Db> {
        std::fs::create_dir_all(data_dir).ok();
        let path = data_dir.join("schemas.db");
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS schemas (
                 name TEXT PRIMARY KEY,
                 owner_id TEXT NOT NULL,
                 owner_alias TEXT NOT NULL,
                 select_keys TEXT NOT NULL,
                 max_len INTEGER NOT NULL,
                 punct_key TEXT NOT NULL,
                 custom_font TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS codes (
                 schema_name TEXT NOT NULL,
                 word TEXT NOT NULL,
                 code TEXT NOT NULL,
                 pos INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_codes_word ON codes(schema_name, word);
             CREATE INDEX IF NOT EXISTS idx_codes_code ON codes(schema_name, code);
             CREATE TABLE IF NOT EXISTS freqs (
                 schema_name TEXT NOT NULL,
                 word TEXT NOT NULL,
                 freq TEXT NOT NULL
             );",
        )?;
        Ok(Db { conn })
    }

    /// 导入方案 + 码表（覆盖同名方案的码表）。返回导入条数。
    pub fn import_schema(
        &mut self,
        name: &str,
        select_keys: &str,
        max_len: i64,
        tsv: &str,
    ) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO schemas(name,owner_id,owner_alias,select_keys,max_len,punct_key,custom_font)
             VALUES(?1,'server','server',?2,?3,'','')
             ON CONFLICT(name) DO UPDATE SET select_keys=?2, max_len=?3",
            params![name, select_keys, max_len],
        )?;
        tx.execute("DELETE FROM codes WHERE schema_name=?1", params![name])?;
        let entries = parse_tsv(tsv);
        let rows = assign_positions(&entries);
        for (code, word, pos) in &rows {
            tx.execute(
                "INSERT INTO codes(schema_name,word,code,pos) VALUES(?1,?2,?3,?4)",
                params![name, word, code, pos],
            )?;
        }
        tx.commit()?;
        Ok(rows.len())
    }

    /// 导入词频表（覆盖同名）。
    pub fn import_freq(&mut self, name: &str, tsv: &str) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM freqs WHERE schema_name=?1", params![name])?;
        let entries = parse_freq_tsv(tsv);
        for (word, freq) in &entries {
            tx.execute(
                "INSERT INTO freqs(schema_name,word,freq) VALUES(?1,?2,?3)",
                params![name, word, freq],
            )?;
        }
        tx.commit()?;
        Ok(entries.len())
    }

    pub fn get_schema(&self, name: &str) -> rusqlite::Result<Option<SchemaInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT name,owner_id,owner_alias,select_keys,max_len,punct_key,custom_font,
                    (SELECT COUNT(*) FROM codes c WHERE c.schema_name=s.name) AS cnt
             FROM schemas s WHERE name=?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(SchemaInfo {
                name: row.get(0)?,
                owner_id: row.get(1)?,
                owner_alias: row.get(2)?,
                select_keys: row.get(3)?,
                max_len: row.get(4)?,
                punct_key: row.get(5)?,
                custom_font: row.get(6)?,
                count: row.get(7)?,
            })
        })?;
        rows.next().transpose()
    }

    /// 一次性读取某方案的全部码表行 (word, code, pos)，用于构建内存缓存。
    pub fn load_codes(&self, name: &str) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT word,code,pos FROM codes WHERE schema_name=?1 ORDER BY word,pos")?;
        let rows = stmt.query_map(params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?;
        rows.collect()
    }

    /// 一次性读取某方案的全部词频 (word, freq)，用于构建内存缓存。
    pub fn load_freqs(&self, name: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT word,freq FROM freqs WHERE schema_name=?1")?;
        let rows = stmt.query_map(params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect()
    }

    /// 列出所有已存在词频表的名称（用于启动时预热缓存）。
    pub fn list_freq_names(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT schema_name FROM freqs ORDER BY schema_name")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// 列出所有已存在码表方案的名称（用于启动时预热缓存）。
    pub fn list_schema_names(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT name FROM schemas ORDER BY name")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }
}
