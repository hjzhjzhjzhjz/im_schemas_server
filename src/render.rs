//! 中文图文渲染：基于 cosmic-text（原生支持 CJK / ttc / 字体回退）。
//! 每个彩色片段用独立 buffer 渲染，按相同 metrics 保证同一行内竖直对齐，再合成到画布。

use crate::segment::{AllResult, Segment, Stats};
use cosmic_text::{Attrs, Buffer, Color, FontSystem, Metrics, Shaping, SwashCache};
use image::{Rgba, RgbaImage};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Condvar, Mutex};

const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/noto/NotoSansSC-Regular.otf",
    "/usr/share/fonts/opentype/noto/NotoSansSC-Regular.ttf",
    "/System/Library/Fonts/PingFang.ttc",
];

pub struct TextEngine {
    inner: Mutex<(FontSystem, SwashCache)>,
}

impl TextEngine {
    pub fn new() -> TextEngine {
        let mut font_system = FontSystem::new();
        for path in FONT_CANDIDATES {
            if Path::new(path).exists() {
                let _ = font_system.db_mut().load_font_file(path);
            }
        }
        let swash = SwashCache::new();
        TextEngine {
            inner: Mutex::new((font_system, swash)),
        }
    }

    fn metrics(size: f32) -> Metrics {
        Metrics::new(size, size * 1.35)
    }

    /// 测量文本包围盒（宽、高，像素）。
    pub fn measure(&self, text: &str, size: f32) -> (u32, u32) {
        let mut g = self.inner.lock().unwrap();
        let (fs, sw) = &mut *g;
        let metrics = Self::metrics(size);
        let mut buffer = Buffer::new(fs, metrics);
        buffer.set_metrics(fs, metrics);
        buffer.set_text(fs, text, Attrs::new(), Shaping::Advanced);
        let mut max_x = 0i32;
        let mut max_y = 0i32;
        buffer.draw(fs, sw, Color::rgb(0, 0, 0), |x, y, w, h, _c| {
            if x + w as i32 > max_x {
                max_x = x + w as i32;
            }
            if y + h as i32 > max_y {
                max_y = y + h as i32;
            }
        });
        (max_x.max(1) as u32, max_y.max(1) as u32)
    }

    /// 在画布 (x, y_top) 处绘制单色文本（y_top 为字形包围盒顶部），按 alpha 与背景混合。
    pub fn draw(&self, img: &mut RgbaImage, x: i32, y_top: i32, text: &str, size: f32, color: (u8, u8, u8)) {
        let mut g = self.inner.lock().unwrap();
        let (fs, sw) = &mut *g;
        let metrics = Self::metrics(size);
        let (img_w, img_h) = img.dimensions();
        let mut buffer = Buffer::new(fs, metrics);
        buffer.set_metrics(fs, metrics);
        let col = Color::rgb(color.0, color.1, color.2);
        buffer.set_text(fs, text, Attrs::new().color(col), Shaping::Advanced);
        buffer.draw(fs, sw, col, |px, py, w, h, c: Color| {
            let a = c.a() as f32 / 255.0;
            let cr = c.r() as f32;
            let cg = c.g() as f32;
            let cb = c.b() as f32;
            for yy in py..py + h as i32 {
                for xx in px..px + w as i32 {
                    let tx = x + xx as i32;
                    let ty = y_top + yy as i32;
                    if tx < 0 || ty < 0 {
                        continue;
                    }
                    let (tx, ty) = (tx as u32, ty as u32);
                    if tx >= img_w || ty >= img_h {
                        continue;
                    }
                    let bg = img.get_pixel(tx, ty);
                    let inv = 1.0 - a;
                    let nr = (cr * a + bg[0] as f32 * inv) as u8;
                    let ng = (cg * a + bg[1] as f32 * inv) as u8;
                    let nb = (cb * a + bg[2] as f32 * inv) as u8;
                    img.put_pixel(tx, ty, Rgba([nr, ng, nb, 255]));
                }
            }
        });
    }
}

fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    if h.len() == 6 {
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
        (r, g, b)
    } else {
        (0, 0, 0)
    }
}

fn fill_rect(img: &mut RgbaImage, x: i32, y: i32, w: u32, h: u32, color: (u8, u8, u8)) {
    for yy in y..y + h as i32 {
        for xx in x..x + w as i32 {
            if xx >= 0 && yy >= 0 {
                let (xx, yy) = (xx as u32, yy as u32);
                if xx < img.width() && yy < img.height() {
                    img.put_pixel(xx, yy, Rgba([color.0, color.1, color.2, 255]));
                }
            }
        }
    }
}

const PAD: i32 = 24;
const TITLE_SIZE: f32 = 22.0;
const MAIN_SIZE: f32 = 48.0;
const SUB_SIZE: f32 = 20.0;

fn stats_line(s: &Stats) -> String {
    let eq = match s.equivalent {
        Some(v) => format!("{:.2}", v),
        None => "-".to_string(),
    };
    format!(
        "按键 {} · 词组 {} · 选重 {} · 当量 {} · 缺字 {}",
        s.total_keys, s.phrase_count, s.select_count, eq, s.missing_count
    )
}

/// 查询主图：标题统计 + 彩色分段文本 + 可选键盘热力图。
pub fn render_query(
    engine: &TextEngine,
    segments: &[Segment],
    stats: &Stats,
    show_keyboard: bool,
) -> RgbaImage {
    let title = stats_line(stats);
    let (title_w, _) = engine.measure(&title, TITLE_SIZE);

    let mut main_w = 0i32;
    let mut widths: Vec<i32> = Vec::new();
    for seg in segments {
        let (w, _) = engine.measure(&seg.text, MAIN_SIZE);
        let w = w as i32;
        widths.push(w);
        main_w += w + 14;
    }
    main_w = main_w.saturating_sub(14);

    let (kb_w, kb_h) = if show_keyboard {
        keyboard_size()
    } else {
        (0, 0)
    };
    let right_block = if show_keyboard {
        main_w + 60 + kb_w
    } else {
        main_w
    };

    let content_w = (title_w as i32).max(right_block) + PAD + 8;
    let content_h = PAD + TITLE_SIZE as i32 + 30 + MAIN_SIZE as i32 + 20
        + if show_keyboard { kb_h } else { 0 }
        + PAD;

    let mut img = RgbaImage::new(content_w as u32, content_h as u32);
    for p in img.pixels_mut() {
        *p = Rgba([255, 255, 255, 255]);
    }

    let title_y = PAD;
    engine.draw(&mut img, PAD, title_y, &title, TITLE_SIZE, (60, 60, 60));

    let main_y = PAD + TITLE_SIZE as i32 + 30;
    let mut x = PAD;
    for (seg, w) in segments.iter().zip(widths.iter()) {
        let rgb = hex_to_rgb(&seg.color);
        engine.draw(&mut img, x, main_y, &seg.text, MAIN_SIZE, rgb);
        x += w + 14;
    }

    if show_keyboard {
        draw_keyboard(engine, &mut img, (PAD + main_w + 60) as i32, PAD, &stats.key_counts);
    }

    img
}

/// 单字图：大字 + 候选编码列表（选重位序≥2 标红）。
pub fn render_single_char(engine: &TextEngine, seg: &Segment) -> RgbaImage {
    let big = 120.0;
    let col_w = 150i32;
    let line_h = 34i32;
    let n = seg.codes.len().max(1) as i32;
    let w = col_w + 360;
    let h = PAD + (big as i32).max(n * line_h + 10) + PAD;
    let mut img = RgbaImage::new(w as u32, h as u32);
    for p in img.pixels_mut() {
        *p = Rgba([255, 255, 255, 255]);
    }
    engine.draw(&mut img, PAD, PAD, &seg.text, big, hex_to_rgb(&seg.color));
    let mut ly = PAD + SUB_SIZE as i32;
    if seg.missing {
        engine.draw(&mut img, col_w, ly, "（缺字）", SUB_SIZE, (142, 36, 170));
    } else {
        for (i, ce) in seg.codes.iter().enumerate() {
            let label = format!("{}. {}", ce.pos, ce.code);
            let color = if ce.pos > 1 { (229, 57, 53) } else { (0, 0, 0) };
            engine.draw(&mut img, col_w, ly, &label, SUB_SIZE, color);
            ly += line_h;
            if i >= 11 {
                break;
            }
        }
    }
    img
}

/// 反查图：编码 -> 候选字词列表。
pub fn render_reverse(engine: &TextEngine, code: &str, words: &[(String, i64)]) -> RgbaImage {
    let header = format!("反查 {}", code);
    let line_h = 34i32;
    let n = words.len().max(1) as i32;
    let w = 520i32;
    let h = PAD + (SUB_SIZE as i32) + 10 + n * line_h + PAD;
    let mut img = RgbaImage::new(w as u32, h as u32);
    for p in img.pixels_mut() {
        *p = Rgba([255, 255, 255, 255]);
    }
    let mut y = PAD;
    engine.draw(&mut img, PAD, y, &header, SUB_SIZE, (60, 60, 60));
    y += 10 + line_h;
    if words.is_empty() {
        engine.draw(&mut img, PAD, y, "（无匹配）", SUB_SIZE, (142, 36, 170));
    }
    for (word, pos) in words {
        let color = if *pos > 1 { (229, 57, 53) } else { (0, 0, 0) };
        let label = format!("{}. {}", pos, word);
        engine.draw(&mut img, PAD, y, &label, SUB_SIZE, color);
        y += line_h;
    }
    img
}

/// all 对比图：每个方案一段（名 + 统计 + 彩色文本）。
pub fn render_all(engine: &TextEngine, results: &[AllResult], text: &str) -> RgbaImage {
    let header = format!("all 对比：{}", text);
    let (hw, _) = engine.measure(&header, TITLE_SIZE);
    let line_h = (MAIN_SIZE * 1.2) as i32;
    let block_h = (TITLE_SIZE as i32) + 8 + SUB_SIZE as i32 + 6 + line_h + 20;
    let h = PAD + (TITLE_SIZE as i32) + 16 + results.len() as i32 * block_h + PAD;
    let mut max_w = hw as i32 + PAD;
    for r in results {
        let mut bw = 0i32;
        for seg in &r.segments {
            let (w, _) = engine.measure(&seg.text, MAIN_SIZE);
            bw += w as i32 + 14;
        }
        max_w = max_w.max(bw + PAD);
    }
    let w = max_w.max(400);
    let mut img = RgbaImage::new(w as u32, h as u32);
    for p in img.pixels_mut() {
        *p = Rgba([255, 255, 255, 255]);
    }
    let mut y = PAD;
    engine.draw(&mut img, PAD, y, &header, TITLE_SIZE, (60, 60, 60));
    y += 16;
    for r in results {
        engine.draw(&mut img, PAD, y, &r.schema, TITLE_SIZE, (21, 101, 192));
        let sy = y + TITLE_SIZE as i32 + 8;
        engine.draw(&mut img, PAD + 8, sy, &stats_line(&r.stats), SUB_SIZE, (90, 90, 90));
        let seg_y = sy + SUB_SIZE as i32 + 6;
        let mut x = PAD + 8;
        for seg in &r.segments {
            let (sw, _) = engine.measure(&seg.text, MAIN_SIZE);
            engine.draw(&mut img, x, seg_y, &seg.text, MAIN_SIZE, hex_to_rgb(&seg.color));
            x += sw as i32 + 14;
        }
        y += block_h;
    }
    img
}

// ─────────────── 键盘热力图 ───────────────

const KEY_ROWS: &[&str] = &["qwertyuiop", "asdfghjkl", "zxcvbnm"];

fn keyboard_size() -> (i32, i32) {
    let cell = 28i32;
    let gap = 4i32;
    let kb_w = "qwertyuiop".len() as i32 * (cell + gap);
    let kb_h = KEY_ROWS.len() as i32 * (cell + gap) + (cell + gap); // + 空格行
    (kb_w, kb_h)
}

fn draw_keyboard(
    engine: &TextEngine,
    img: &mut RgbaImage,
    x0: i32,
    y0: i32,
    key_counts: &HashMap<char, usize>,
) {
    let cell = 28i32;
    let gap = 4i32;
    let max_count = key_counts.values().cloned().max().unwrap_or(1).max(1) as f32;
    let red = (229, 57, 53);
    let mut row_idx = 0;
    for row in KEY_ROWS {
        let mut x = x0;
        let y = y0 + row_idx * (cell + gap);
        for ch in row.chars() {
            let t = key_counts.get(&ch).copied().unwrap_or(0) as f32 / max_count;
            let t = t.min(1.0);
            let color = (
                (255.0 + (red.0 as f32 - 255.0) * t) as u8,
                (255.0 + (red.1 as f32 - 255.0) * t) as u8,
                (255.0 + (red.2 as f32 - 255.0) * t) as u8,
            );
            fill_rect(img, x, y, cell as u32, cell as u32, color);
            let label = ch.to_string();
            let (lw, _) = engine.measure(&label, 16.0);
            engine.draw(
                img,
                x + (cell - lw as i32) / 2,
                y + (cell * 3 / 4),
                &label,
                16.0,
                (0, 0, 0),
            );
            x += cell + gap;
        }
        row_idx += 1;
    }
    let y = y0 + row_idx * (cell + gap);
    let sp_w = "qwertyuiop".len() as i32 * (cell + gap) - gap;
    let t = key_counts.get(&' ').copied().unwrap_or(0) as f32 / max_count;
    let t = t.min(1.0);
    let color = (
        (255.0 + (red.0 as f32 - 255.0) * t) as u8,
        (255.0 + (red.1 as f32 - 255.0) * t) as u8,
        (255.0 + (red.2 as f32 - 255.0) * t) as u8,
    );
    fill_rect(img, x0, y, sp_w as u32, cell as u32, color);
    let (lw, _) = engine.measure("␣", 16.0);
    engine.draw(img, x0 + (sp_w - lw as i32) / 2, y + (cell * 3 / 4), "␣", 16.0, (0, 0, 0));
}

// ─────────────── 渲染引擎池 ───────────────
// 单个 TextEngine 内部用 Mutex 串行化 FontSystem/SwashCache；并发渲染若共用一个引擎
// 会全部卡在同一把锁上。这里按需创建多个独立引擎，请求独占借用 → 真正并行。

/// 引擎池：并发上限内按需创建，空闲引擎复用，超出上限时阻塞等待。
pub struct EnginePool {
    state: Mutex<PoolState>,
    cond: Condvar,
}

struct PoolState {
    idle: Vec<TextEngine>,
    total: usize,
    max: usize,
}

/// 借出的引擎守卫，Drop 时自动归还（即使渲染 panic 也不泄漏）。
pub struct PooledEngine<'a> {
    pool: &'a EnginePool,
    engine: Option<TextEngine>,
}

impl EnginePool {
    pub fn new(max: usize) -> Self {
        EnginePool {
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                total: 0,
                max: max.max(1),
            }),
            cond: Condvar::new(),
        }
    }

    pub fn size(&self) -> usize {
        self.state.lock().unwrap().max
    }

    pub fn acquire(&self) -> PooledEngine<'_> {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(e) = st.idle.pop() {
                return PooledEngine { pool: self, engine: Some(e) };
            }
            if st.total < st.max {
                st.total += 1;
                drop(st); // FontSystem::new() 很重，锁外创建，避免阻塞其他线程
                return PooledEngine { pool: self, engine: Some(TextEngine::new()) };
            }
            st = self.cond.wait(st).unwrap();
        }
    }

    fn release(&self, e: TextEngine) {
        let mut st = self.state.lock().unwrap();
        st.idle.push(e);
        drop(st);
        self.cond.notify_one();
    }
}

impl<'a> std::ops::Deref for PooledEngine<'a> {
    type Target = TextEngine;
    fn deref(&self) -> &TextEngine {
        self.engine.as_ref().expect("pooled engine")
    }
}

impl<'a> Drop for PooledEngine<'a> {
    fn drop(&mut self) {
        if let Some(e) = self.engine.take() {
            self.pool.release(e);
        }
    }
}
