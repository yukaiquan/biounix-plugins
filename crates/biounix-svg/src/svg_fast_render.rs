//! 混合流式 SVG 渲染器（针对超大 SVG）
//!
//! usvg 的 DOM 解析器（roxmltree）在处理 416MB、数百万元素的 SVG 时
//! 会耗尽内存（>2.7GB）且长时间无法完成解析。
//!
//! 本模块使用两遍流式策略：
//! 1. 第一遍流式扫描：提取 `<style>` CSS + `<clipPath>` 矩形 + SVG 尺寸
//! 2. 第二遍流式渲染：quick-xml 事件驱动 + tiny-skia 直接绘制
//!    - rect → fill_rect（含 clip）
//!    - path → PathBuilder 解析 d 属性 → fill_path（含 clip）
//!    - text/tspan → ab_glyph 字体渲染
//!    - g → 维护 fill/clip/opacity/transform 栈
//! 3. 编码输出（复用 svg_convert::encode_pixmap）
//!
//! 不支持的特性（静默跳过 + 收集警告）：
//! 渐变、滤镜、pattern、mask、image、非矩形 clipPath、复杂文字布局、arc 近似

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};

use ab_glyph::{Font, FontVec, GlyphId, OutlineCurve, Point as AbPoint, ScaleFont};
use quick_xml::events::attributes::Attributes;
use quick_xml::events::Event;
use quick_xml::Reader;
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, Pixmap, Rect, Transform};

use crate::svg_convert::{
    compute_target_size, encode_pixmap, parse_hex_color, parse_length, SvgConvertOptions,
    SvgConvertResult,
};

// ============ 数据结构 ============

/// CSS 类样式
#[derive(Default, Clone)]
struct CssClass {
    fill: Option<(u8, u8, u8)>,
    fill_set: bool, // fill 是否被显式设置（包括 fill:none）
    fill_opacity: Option<f32>,
    stroke: Option<(u8, u8, u8)>,
    stroke_set: bool, // stroke 是否被显式设置（包括 stroke:none）
    stroke_opacity: Option<f32>,
    stroke_width: Option<f32>,
    opacity: Option<f32>,
    font_family: Option<String>,
    font_size: Option<f32>,
    font_weight: Option<u16>,
    font_style: Option<String>,
    clip_path: Option<String>,
    display_none: bool,
}

/// clipPath 中的矩形
#[derive(Clone)]
struct ClipRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// 渲染上下文（栈帧）
#[derive(Clone)]
struct RenderCtx {
    fill: Option<(u8, u8, u8)>,
    fill_opacity: f32,
    stroke: Option<(u8, u8, u8)>,
    stroke_opacity: f32,
    stroke_width: f32,
    opacity: f32,
    font_family: String,
    font_size: f32,
    font_weight: u16,
    font_style: String,
    clip_path: Option<String>,
    transform: Transform,
    display_none: bool,
}

impl Default for RenderCtx {
    fn default() -> Self {
        RenderCtx {
            fill: Some((0, 0, 0)),
            fill_opacity: 1.0,
            stroke: None,
            stroke_opacity: 1.0,
            stroke_width: 1.0,
            opacity: 1.0,
            font_family: "Arial".to_string(),
            font_size: 12.0,
            font_weight: 400,
            font_style: "normal".to_string(),
            clip_path: None,
            transform: Transform::identity(),
            display_none: false,
        }
    }
}

/// 文本渲染状态
#[derive(Default)]
struct TextState {
    active: bool,
    transform: Transform,
    font_family: String,
    font_size: f32,
    font_style: String,
    font_weight: u16,
    fill: Option<(u8, u8, u8)>,
    opacity: f32,
    tspan_x: Option<f32>,
    tspan_y: Option<f32>,
    text_buf: String,
}

// ============ CSS 解析 ============

/// 解析 CSS 文本（如 `.st0{fill:#1A1A1A}.st1{clip-path:url(#x)}`）
fn parse_css(css: &str) -> HashMap<String, CssClass> {
    let mut map: HashMap<String, CssClass> = HashMap::new();
    let bytes = css.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 找 '.' 或其他选择器起始
        while i < bytes.len() && bytes[i] != b'.' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // 收集本规则块的所有类名（逗号分隔的多选择器）
        let mut class_names: Vec<String> = Vec::new();
        loop {
            // 当前在 '.' 位置
            i += 1; // skip '.'
            let name_start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
            {
                i += 1;
            }
            let name = &css[name_start..i];
            if !name.is_empty() {
                class_names.push(name.to_string());
            }
            // 跳过空白和逗号，看是否还有下一个 '.'
            while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'.' {
                break; // 没有更多选择器
            }
            // bytes[i] == '.', 继续收集下一个类名
        }
        // 跳到 '{'
        while i < bytes.len() && bytes[i] != b'{' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        i += 1; // skip '{'
                // 读声明直到 '}'
        let decl_start = i;
        while i < bytes.len() && bytes[i] != b'}' {
            i += 1;
        }
        let decl = &css[decl_start..i];
        if i < bytes.len() {
            i += 1; // skip '}'
        }
        // 解析声明
        let mut class = CssClass::default();
        for part in decl.split(';') {
            let part = part.trim();
            if let Some(colon) = part.find(':') {
                let key = part[..colon].trim();
                let val = part[colon + 1..].trim();
                apply_css_decl(&mut class, key, val);
            }
        }
        // 将声明应用到所有类名（多选择器）
        for name in &class_names {
            // 合并：如果类已存在，合并属性（后定义的覆盖先定义的）
            map.entry(name.clone())
                .and_modify(|existing| {
                    merge_css_class(existing, &class);
                })
                .or_insert_with(|| class.clone());
        }
    }
    map
}

/// 合并 CSS class 属性（新值覆盖旧值，None 不覆盖）
fn merge_css_class(existing: &mut CssClass, new: &CssClass) {
    if new.fill_set {
        existing.fill = new.fill;
        existing.fill_set = true;
    }
    if new.fill_opacity.is_some() {
        existing.fill_opacity = new.fill_opacity;
    }
    if new.stroke_set {
        existing.stroke = new.stroke;
        existing.stroke_set = true;
    }
    if new.stroke_opacity.is_some() {
        existing.stroke_opacity = new.stroke_opacity;
    }
    if new.stroke_width.is_some() {
        existing.stroke_width = new.stroke_width;
    }
    if new.opacity.is_some() {
        existing.opacity = new.opacity;
    }
    if new.font_family.is_some() {
        existing.font_family = new.font_family.clone();
    }
    if new.font_size.is_some() {
        existing.font_size = new.font_size;
    }
    if new.font_weight.is_some() {
        existing.font_weight = new.font_weight;
    }
    if new.font_style.is_some() {
        existing.font_style = new.font_style.clone();
    }
    if new.clip_path.is_some() {
        existing.clip_path = new.clip_path.clone();
    }
    if new.display_none {
        existing.display_none = true;
    }
}

fn apply_css_decl(class: &mut CssClass, key: &str, val: &str) {
    match key {
        "fill" => {
            class.fill_set = true;
            if val.eq_ignore_ascii_case("none") {
                class.fill = None;
            } else {
                class.fill = parse_css_color(val);
            }
        }
        "fill-opacity" => class.fill_opacity = val.parse::<f32>().ok(),
        "stroke" => {
            class.stroke_set = true;
            if val.eq_ignore_ascii_case("none") {
                class.stroke = None;
            } else {
                class.stroke = parse_css_color(val);
            }
        }
        "stroke-opacity" => class.stroke_opacity = val.parse::<f32>().ok(),
        "stroke-width" => class.stroke_width = parse_length_px(val),
        "opacity" => class.opacity = val.parse::<f32>().ok(),
        "font-family" => class.font_family = Some(val.trim_matches('"').to_string()),
        "font-size" => class.font_size = parse_length_px(val),
        "font-weight" => {
            class.font_weight = match val {
                "bold" => Some(700),
                "normal" => Some(400),
                _ => val.parse::<u16>().ok(),
            };
        }
        "font-style" => class.font_style = Some(val.to_string()),
        "clip-path" => {
            if let Some(id) = extract_url(val) {
                class.clip_path = Some(id);
            }
        }
        "display" => {
            if val == "none" {
                class.display_none = true;
            }
        }
        _ => {} // 忽略未知属性
    }
}

fn parse_length_px(val: &str) -> Option<f32> {
    let v = val
        .trim_end_matches("px")
        .trim_end_matches("pt")
        .trim_end_matches("em");
    v.parse::<f32>().ok()
}

fn extract_url(val: &str) -> Option<String> {
    let val = val.trim();
    if let Some(start) = val.find("url(#") {
        let rest = &val[start + 5..];
        if let Some(end) = rest.find(')') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn parse_css_color(val: &str) -> Option<(u8, u8, u8)> {
    let val = val.trim();
    if val.starts_with('#') {
        let (r, g, b, _) = parse_hex_color(val)?;
        return Some((r, g, b));
    }
    if val.starts_with("rgb(") && val.ends_with(')') {
        let inner = &val[4..val.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 3 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            return Some((r, g, b));
        }
    }
    match val.to_lowercase().as_str() {
        "black" => Some((0, 0, 0)),
        "white" => Some((255, 255, 255)),
        "red" => Some((255, 0, 0)),
        "green" => Some((0, 128, 0)),
        "blue" => Some((0, 0, 255)),
        "yellow" => Some((255, 255, 0)),
        "none" => None,
        _ => None,
    }
}

// ============ transform 解析 ============

fn parse_transform(s: &str) -> Transform {
    let s = s.trim();
    let mut result = Transform::identity();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name = &s[name_start..i];
        while i < bytes.len() && bytes[i] != b'(' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        i += 1; // skip '('
        let args_start = i;
        while i < bytes.len() && bytes[i] != b')' {
            i += 1;
        }
        let args_str = &s[args_start..i];
        if i < bytes.len() {
            i += 1; // skip ')'
        }
        let args: Vec<f32> = args_str
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f32>().ok())
            .collect();

        let op_t = match name {
            "matrix" => {
                if args.len() == 6 {
                    Transform::from_row(args[0], args[1], args[2], args[3], args[4], args[5])
                } else {
                    Transform::identity()
                }
            }
            "translate" => {
                let tx = args.get(0).copied().unwrap_or(0.0);
                let ty = args.get(1).copied().unwrap_or(0.0);
                Transform::from_translate(tx, ty)
            }
            "scale" => {
                let sx = args.get(0).copied().unwrap_or(1.0);
                let sy = args.get(1).copied().unwrap_or(sx);
                Transform::from_scale(sx, sy)
            }
            "rotate" => {
                let angle = args.get(0).copied().unwrap_or(0.0);
                let cx = args.get(1).copied().unwrap_or(0.0);
                let cy = args.get(2).copied().unwrap_or(0.0);
                let rad = angle.to_radians();
                let cos = rad.cos();
                let sin = rad.sin();
                if cx != 0.0 || cy != 0.0 {
                    Transform::from_translate(cx, cy)
                        .pre_concat(Transform::from_row(cos, sin, -sin, cos, 0.0, 0.0))
                        .pre_concat(Transform::from_translate(-cx, -cy))
                } else {
                    Transform::from_row(cos, sin, -sin, cos, 0.0, 0.0)
                }
            }
            "skewX" => {
                let angle = args.get(0).copied().unwrap_or(0.0);
                let t = angle.to_radians().tan();
                Transform::from_row(1.0, 0.0, t, 1.0, 0.0, 0.0)
            }
            "skewY" => {
                let angle = args.get(0).copied().unwrap_or(0.0);
                let t = angle.to_radians().tan();
                Transform::from_row(1.0, t, 0.0, 1.0, 0.0, 0.0)
            }
            _ => Transform::identity(),
        };
        result = result.pre_concat(op_t);
    }
    result
}

// ============ path d 解析（不依赖 svgtypes，自行实现）============

// 注：tiny-skia 的 stroke_path 内部会先用 stroke.width 生成描边路径，
// 再用传入的 transform 变换该路径（宽度也一起被缩放）。
// 因此 sp.width 应保持 SVG 用户单位值，不要手动乘 transform 缩放因子，
// 否则高 DPI 时线条粗细会被放大 scale² 倍（变粗）。

fn build_path(d: &str) -> Result<tiny_skia::Path, String> {
    let mut pb = PathBuilder::new();
    let mut i = 0;
    let mut cur_x = 0.0f32;
    let mut cur_y = 0.0f32;
    let mut start_x = 0.0f32;
    let mut start_y = 0.0f32;
    let mut last_cmd = 0u8;
    let mut last_ctrl_x = 0.0f32;
    let mut last_ctrl_y = 0.0f32;
    let mut last_was_curve = false;

    let skip_ws = |b: &[u8], i: &mut usize| {
        while *i < b.len()
            && (b[*i] == b' '
                || b[*i] == b','
                || b[*i] == b'\t'
                || b[*i] == b'\n'
                || b[*i] == b'\r')
        {
            *i += 1;
        }
    };
    let parse_num = |b: &[u8], i: &mut usize| -> Option<f32> {
        skip_ws(b, i);
        if *i >= b.len() {
            return None;
        }
        let start = *i;
        if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
            *i += 1;
        }
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i < b.len() && b[*i] == b'.' {
            *i += 1;
            while *i < b.len() && b[*i].is_ascii_digit() {
                *i += 1;
            }
        }
        if *i < b.len() && (b[*i] == b'e' || b[*i] == b'E') {
            *i += 1;
            if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
                *i += 1;
            }
            while *i < b.len() && b[*i].is_ascii_digit() {
                *i += 1;
            }
        }
        if *i > start {
            std::str::from_utf8(&b[start..*i])
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
        } else {
            None
        }
    };

    let bytes = d.as_bytes();
    while i < bytes.len() {
        skip_ws(bytes, &mut i);
        if i >= bytes.len() {
            break;
        }
        let c = bytes[i];
        if c.is_ascii_alphabetic() {
            last_cmd = c;
            i += 1;
        }
        let abs = last_cmd.is_ascii_uppercase();
        match last_cmd.to_ascii_uppercase() {
            b'M' => {
                if let (Some(x), Some(y)) = (parse_num(bytes, &mut i), parse_num(bytes, &mut i)) {
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    cur_x = nx;
                    cur_y = ny;
                    start_x = nx;
                    start_y = ny;
                    pb.move_to(nx, ny);
                    // 后续隐式命令为 L
                    last_cmd = if abs { b'L' } else { b'l' };
                }
                last_was_curve = false;
            }
            b'L' => {
                if let (Some(x), Some(y)) = (parse_num(bytes, &mut i), parse_num(bytes, &mut i)) {
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    cur_x = nx;
                    cur_y = ny;
                    pb.line_to(nx, ny);
                }
                last_was_curve = false;
            }
            b'H' => {
                if let Some(x) = parse_num(bytes, &mut i) {
                    let nx = if abs { x } else { cur_x + x };
                    cur_x = nx;
                    pb.line_to(nx, cur_y);
                }
                last_was_curve = false;
            }
            b'V' => {
                if let Some(y) = parse_num(bytes, &mut i) {
                    let ny = if abs { y } else { cur_y + y };
                    cur_y = ny;
                    pb.line_to(cur_x, ny);
                }
                last_was_curve = false;
            }
            b'C' => {
                if let (Some(x1), Some(y1), Some(x2), Some(y2), Some(x), Some(y)) = (
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                ) {
                    let (nx1, ny1) = if abs {
                        (x1, y1)
                    } else {
                        (cur_x + x1, cur_y + y1)
                    };
                    let (nx2, ny2) = if abs {
                        (x2, y2)
                    } else {
                        (cur_x + x2, cur_y + y2)
                    };
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    pb.cubic_to(nx1, ny1, nx2, ny2, nx, ny);
                    last_ctrl_x = nx2;
                    last_ctrl_y = ny2;
                    cur_x = nx;
                    cur_y = ny;
                }
                last_was_curve = true;
            }
            b'S' => {
                if let (Some(x2), Some(y2), Some(x), Some(y)) = (
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                ) {
                    let (nx1, ny1) = if last_was_curve {
                        (2.0 * cur_x - last_ctrl_x, 2.0 * cur_y - last_ctrl_y)
                    } else {
                        (cur_x, cur_y)
                    };
                    let (nx2, ny2) = if abs {
                        (x2, y2)
                    } else {
                        (cur_x + x2, cur_y + y2)
                    };
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    pb.cubic_to(nx1, ny1, nx2, ny2, nx, ny);
                    last_ctrl_x = nx2;
                    last_ctrl_y = ny2;
                    cur_x = nx;
                    cur_y = ny;
                }
                last_was_curve = true;
            }
            b'Q' => {
                if let (Some(x1), Some(y1), Some(x), Some(y)) = (
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                    parse_num(bytes, &mut i),
                ) {
                    let (nx1, ny1) = if abs {
                        (x1, y1)
                    } else {
                        (cur_x + x1, cur_y + y1)
                    };
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    pb.quad_to(nx1, ny1, nx, ny);
                    last_ctrl_x = nx1;
                    last_ctrl_y = ny1;
                    cur_x = nx;
                    cur_y = ny;
                }
                last_was_curve = true;
            }
            b'T' => {
                if let (Some(x), Some(y)) = (parse_num(bytes, &mut i), parse_num(bytes, &mut i)) {
                    let (nx1, ny1) = if last_was_curve {
                        (2.0 * cur_x - last_ctrl_x, 2.0 * cur_y - last_ctrl_y)
                    } else {
                        (cur_x, cur_y)
                    };
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    pb.quad_to(nx1, ny1, nx, ny);
                    last_ctrl_x = nx1;
                    last_ctrl_y = ny1;
                    cur_x = nx;
                    cur_y = ny;
                }
                last_was_curve = true;
            }
            b'A' => {
                // arc: rx ry x-rot large-arc sweep x y（7 参数）
                // 简化：用直线连接起点终点
                let _ = parse_num(bytes, &mut i);
                let _ = parse_num(bytes, &mut i);
                let _ = parse_num(bytes, &mut i);
                let _ = parse_num(bytes, &mut i);
                let _ = parse_num(bytes, &mut i);
                if let (Some(x), Some(y)) = (parse_num(bytes, &mut i), parse_num(bytes, &mut i)) {
                    let (nx, ny) = if abs { (x, y) } else { (cur_x + x, cur_y + y) };
                    pb.line_to(nx, ny);
                    cur_x = nx;
                    cur_y = ny;
                }
                last_was_curve = false;
            }
            b'Z' => {
                pb.close();
                cur_x = start_x;
                cur_y = start_y;
            }
            _ => {
                i += 1;
            }
        }
    }
    pb.finish().ok_or_else(|| "空路径".to_string())
}

// ============ 字体 + 文字渲染 ============

fn load_font() -> Option<ab_glyph::FontVec> {
    let candidates: &[&str] = &[
        // macOS
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        // Windows
        "C:\\Windows\\Fonts\\arial.ttf",
        "C:\\Windows\\Fonts\\arialbd.ttf",
    ];
    for p in candidates {
        if let Ok(data) = std::fs::read(p) {
            if let Ok(font) = FontVec::try_from_vec(data) {
                return Some(font);
            }
        }
    }
    None
}

fn blend_pixel(pixmap: &mut Pixmap, x: u32, y: u32, color: (u8, u8, u8), a: u8) {
    let w = pixmap.width();
    if x >= w || y >= pixmap.height() {
        return;
    }
    let idx = ((y * w + x) * 4) as usize;
    let data = pixmap.data_mut();
    let af = a as f32 / 255.0;
    let inv = 1.0 - af;
    data[idx] = (color.0 as f32 * af + data[idx] as f32 * inv) as u8;
    data[idx + 1] = (color.1 as f32 * af + data[idx + 1] as f32 * inv) as u8;
    data[idx + 2] = (color.2 as f32 * af + data[idx + 2] as f32 * inv) as u8;
    data[idx + 3] = 255;
}

fn render_text(
    pixmap: &mut Pixmap,
    font: &ab_glyph::FontVec,
    text: &str,
    origin_x: f32,
    origin_y: f32,
    font_size: f32,
    color: (u8, u8, u8),
    opacity: f32,
    transform: Transform,
    font_style: &str,
) {
    if font_size <= 0.0 || text.is_empty() {
        return;
    }
    // 用 ab_glyph outline 曲线转 tiny-skia Path，由 tiny-skia 在目标分辨率
    // 应用完整 transform（含旋转/倾斜/缩放）填充，保留旋转且 AA 质量高。
    let scaled = font.as_scaled(font_size);
    let h_factor = scaled.h_scale_factor();
    let v_factor = scaled.v_scale_factor();
    let alpha = (opacity * 255.0).clamp(0.0, 255.0) as u8;
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, alpha);
    // italic 合成斜体：绕 baseline 做 shear（向右倾斜 ≈14°）
    let is_italic =
        font_style.eq_ignore_ascii_case("italic") || font_style.eq_ignore_ascii_case("oblique");
    let py = origin_y; // baseline
    let shear = if is_italic {
        // 绕 baseline (0, py) shear：translate(0,-py) → shear → translate(0,py)
        Transform::from_row(1.0, 0.0, -0.25, 1.0, 0.0, 0.0)
            .pre_translate(0.0, -py)
            .post_translate(0.0, py)
    } else {
        Transform::identity()
    };
    let final_transform = transform.pre_concat(shear);
    let mut px = origin_x;
    for ch in text.chars() {
        let glyph_id: GlyphId = scaled.glyph_id(ch);
        if glyph_id.0 == 0 {
            px += scaled.h_advance(glyph_id);
            continue;
        }
        if let Some(outline) = font.outline(glyph_id) {
            let mut pb = PathBuilder::new();
            let mut last: Option<(f32, f32)> = None;
            // ab_glyph Point y 向上；SVG/tiny-skia y 向下 → 翻转 y
            let to_svg = |p: &AbPoint| -> (f32, f32) { (px + p.x * h_factor, py - p.y * v_factor) };
            for curve in &outline.curves {
                match curve {
                    OutlineCurve::Line(p0, p1) => {
                        let s = to_svg(p0);
                        let e = to_svg(p1);
                        if last != Some(s) {
                            pb.move_to(s.0, s.1);
                        }
                        pb.line_to(e.0, e.1);
                        last = Some(e);
                    }
                    OutlineCurve::Quad(p0, p1, p2) => {
                        let s = to_svg(p0);
                        let c = to_svg(p1);
                        let e = to_svg(p2);
                        if last != Some(s) {
                            pb.move_to(s.0, s.1);
                        }
                        pb.quad_to(c.0, c.1, e.0, e.1);
                        last = Some(e);
                    }
                    OutlineCurve::Cubic(p0, p1, p2, p3) => {
                        let s = to_svg(p0);
                        let c1 = to_svg(p1);
                        let c2 = to_svg(p2);
                        let e = to_svg(p3);
                        if last != Some(s) {
                            pb.move_to(s.0, s.1);
                        }
                        pb.cubic_to(c1.0, c1.1, c2.0, c2.1, e.0, e.1);
                        last = Some(e);
                    }
                }
            }
            pb.close();
            if let Some(path) = pb.finish() {
                pixmap.fill_path(&path, &paint, FillRule::Winding, final_transform, None);
            }
        }
        px += scaled.h_advance(glyph_id);
    }
}

// ============ 属性辅助 ============

/// 一次性提取所有属性到 HashMap（避免多次 clone+iterate）
fn extract_attrs(attrs: &Attributes) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for attr in attrs.clone() {
        if let Ok(attr) = attr {
            if let Ok(key) = std::str::from_utf8(attr.key.as_ref()) {
                if let Ok(val) = attr.unescape_value() {
                    map.insert(key.to_string(), val.into_owned());
                }
            }
        }
    }
    map
}

/// 从已提取的属性 map 获取值
fn attr_get<'a>(map: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    map.get(name).map(|s| s.as_str())
}

fn attr_get_f32(map: &HashMap<String, String>, name: &str) -> Option<f32> {
    attr_get(map, name).and_then(|s| s.trim().parse().ok())
}

/// 把 class 列表应用到 ctx
fn apply_classes(ctx: &mut RenderCtx, classes: &str, css: &HashMap<String, CssClass>) {
    for cname in classes.split_whitespace() {
        if let Some(class) = css.get(cname) {
            if class.fill_set {
                ctx.fill = class.fill;
            }
            if class.fill_opacity.is_some() {
                ctx.fill_opacity = class.fill_opacity.unwrap();
            }
            if class.stroke_set {
                ctx.stroke = class.stroke;
            }
            if class.stroke_opacity.is_some() {
                ctx.stroke_opacity = class.stroke_opacity.unwrap();
            }
            if class.stroke_width.is_some() {
                ctx.stroke_width = class.stroke_width.unwrap();
            }
            if class.opacity.is_some() {
                ctx.opacity = class.opacity.unwrap();
            }
            if class.font_family.is_some() {
                ctx.font_family = class.font_family.clone().unwrap();
            }
            if class.font_size.is_some() {
                ctx.font_size = class.font_size.unwrap();
            }
            if class.font_weight.is_some() {
                ctx.font_weight = class.font_weight.unwrap();
            }
            if class.font_style.is_some() {
                ctx.font_style = class.font_style.clone().unwrap();
            }
            if class.clip_path.is_some() {
                ctx.clip_path = class.clip_path.clone();
            }
            if class.display_none {
                ctx.display_none = true;
            }
        }
    }
}

/// 应用元素的直接属性（覆盖 class）
fn apply_attrs(ctx: &mut RenderCtx, amap: &HashMap<String, String>) {
    if let Some(s) = attr_get(amap, "fill") {
        if s.eq_ignore_ascii_case("none") {
            ctx.fill = None;
        } else {
            ctx.fill = parse_css_color(s);
        }
    }
    if let Some(s) = attr_get_f32(amap, "fill-opacity") {
        ctx.fill_opacity = s;
    }
    if let Some(s) = attr_get(amap, "stroke") {
        if s.eq_ignore_ascii_case("none") {
            ctx.stroke = None;
        } else {
            ctx.stroke = parse_css_color(s);
        }
    }
    if let Some(s) = attr_get(amap, "stroke-width") {
        if let Some(v) = parse_length_px(s) {
            ctx.stroke_width = v;
        }
    }
    if let Some(s) = attr_get_f32(amap, "opacity") {
        ctx.opacity = s;
    }
    if let Some(s) = attr_get(amap, "font-family") {
        ctx.font_family = s.trim_matches('"').to_string();
    }
    if let Some(s) = attr_get_f32(amap, "font-size") {
        ctx.font_size = s;
    }
    if let Some(s) = attr_get(amap, "font-style") {
        ctx.font_style = s.to_string();
    }
    if let Some(s) = attr_get(amap, "font-weight") {
        if let Ok(w) = s.trim().parse::<u16>() {
            ctx.font_weight = w;
        } else if s.trim().eq_ignore_ascii_case("bold") {
            ctx.font_weight = 700;
        }
    }
    if let Some(s) = attr_get(amap, "clip-path") {
        if let Some(id) = extract_url(s) {
            ctx.clip_path = Some(id);
        }
    }
    if let Some(s) = attr_get(amap, "display") {
        if s == "none" {
            ctx.display_none = true;
        }
    }
}

// ============ clipMask 辅助 ============

fn make_clip_mask(
    clips: &HashMap<String, Vec<ClipRect>>,
    id: &str,
    width: u32,
    height: u32,
) -> Option<Mask> {
    let rects = clips.get(id)?;
    let mut mask = Mask::new(width, height)?;
    let mut pb = PathBuilder::new();
    for r in rects {
        if let Some(rect) = Rect::from_xywh(r.x, r.y, r.w, r.h) {
            pb.push_rect(rect);
        }
    }
    let path = match pb.finish() {
        Some(p) => p,
        None => return None,
    };
    mask.fill_path(&path, FillRule::Winding, false, Transform::identity());
    Some(mask)
}

fn get_clip_mask<'a>(
    ctx: &RenderCtx,
    clips: &HashMap<String, Vec<ClipRect>>,
    clip_cache: &'a mut HashMap<String, Mask>,
    width: u32,
    height: u32,
) -> Option<&'a Mask> {
    let id = ctx.clip_path.as_ref()?;
    if !clip_cache.contains_key(id) {
        if let Some(mask) = make_clip_mask(clips, id, width, height) {
            clip_cache.insert(id.clone(), mask);
        } else {
            return None;
        }
    }
    clip_cache.get(id)
}

// ============ 第一遍：收集 CSS + clipPath + 尺寸 ============

struct Pass1Result {
    css: HashMap<String, CssClass>,
    clips: HashMap<String, Vec<ClipRect>>,
    svg_w_px: f64,
    svg_h_px: f64,
    phys_w_in: f64,
    phys_h_in: f64,
}

fn pass1(path: &str) -> Result<Pass1Result, String> {
    let file = File::open(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut reader = Reader::from_reader(BufReader::new(file));
    reader.config_mut().trim_text(false);

    let mut css = HashMap::new();
    let mut clips: HashMap<String, Vec<ClipRect>> = HashMap::new();
    let mut svg_w_px = 0.0f64;
    let mut svg_h_px = 0.0f64;
    let mut phys_w_in = 0.0f64;
    let mut phys_h_in = 0.0f64;

    let mut in_style = false;
    let mut style_buf = String::new();
    let mut in_clippath: Option<String> = None;
    let mut clip_count: usize = 0;
    const MAX_CLIPS: usize = 5000;

    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"svg" => {
                        let amap = extract_attrs(&e.attributes());
                        if let Some(w) = attr_get(&amap, "width") {
                            if let Some((px, _unit, inches)) = parse_length(w) {
                                svg_w_px = px;
                                if inches > 0.0 {
                                    phys_w_in = inches;
                                }
                            }
                        }
                        if let Some(h) = attr_get(&amap, "height") {
                            if let Some((px, _unit, inches)) = parse_length(h) {
                                svg_h_px = px;
                                if inches > 0.0 {
                                    phys_h_in = inches;
                                }
                            }
                        }
                        if let Some(vb) = attr_get(&amap, "viewBox") {
                            let parts: Vec<f64> = vb
                                .split_whitespace()
                                .filter_map(|s| s.parse().ok())
                                .collect();
                            if parts.len() == 4 {
                                if svg_w_px == 0.0 {
                                    svg_w_px = parts[2];
                                    // viewBox 无物理单位，按 96dpi 基准换算英寸，
                                    // 这样 keep_physical_size + dpi 才能放大渲染分辨率
                                    if phys_w_in == 0.0 {
                                        phys_w_in = parts[2] / 96.0;
                                    }
                                }
                                if svg_h_px == 0.0 {
                                    svg_h_px = parts[3];
                                    if phys_h_in == 0.0 {
                                        phys_h_in = parts[3] / 96.0;
                                    }
                                }
                            }
                        }
                    }
                    b"style" => {
                        in_style = true;
                        style_buf.clear();
                    }
                    b"clipPath" => {
                        if clip_count < MAX_CLIPS {
                            let amap = extract_attrs(&e.attributes());
                            let id = attr_get(&amap, "id").unwrap_or_default().to_string();
                            in_clippath = Some(id);
                        }
                        // 超过上限后不再收集（超大 SVG 通常 clipPath 未被引用）
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.name();
                if name.as_ref() == b"rect" && in_clippath.is_some() {
                    let amap = extract_attrs(&e.attributes());
                    let r = ClipRect {
                        x: attr_get_f32(&amap, "x").unwrap_or(0.0),
                        y: attr_get_f32(&amap, "y").unwrap_or(0.0),
                        w: attr_get_f32(&amap, "width").unwrap_or(0.0),
                        h: attr_get_f32(&amap, "height").unwrap_or(0.0),
                    };
                    if let Some(id) = &in_clippath {
                        clips.entry(id.clone()).or_default().push(r);
                        clip_count += 1;
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"style" => {
                        if in_style {
                            css = parse_css(&style_buf);
                            in_style = false;
                        }
                    }
                    b"clipPath" => {
                        in_clippath = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_style {
                    if let Ok(s) = t.unescape() {
                        style_buf.push_str(&s);
                    }
                }
            }
            Ok(Event::CData(c)) => {
                if in_style {
                    style_buf.push_str(std::str::from_utf8(c.as_ref()).unwrap_or(""));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML 解析错误: {}", e)),
            _ => {}
        }
    }

    if svg_w_px <= 0.0 {
        svg_w_px = 800.0;
    }
    if svg_h_px <= 0.0 {
        svg_h_px = 600.0;
    }

    Ok(Pass1Result {
        css,
        clips,
        svg_w_px,
        svg_h_px,
        phys_w_in,
        phys_h_in,
    })
}

// ============ 第二遍：流式渲染 ============

fn pass2(
    path: &str,
    pixmap: &mut Pixmap,
    css: &HashMap<String, CssClass>,
    clips: &HashMap<String, Vec<ClipRect>>,
    font: Option<&ab_glyph::FontVec>,
    sx: f32,
    sy: f32,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut reader = Reader::from_reader(BufReader::new(file));
    reader.config_mut().trim_text(false);

    let width = pixmap.width();
    let height = pixmap.height();
    let scale_transform = Transform::from_scale(sx, sy);

    let mut stack: Vec<RenderCtx> = vec![RenderCtx::default()];
    let mut clip_cache: HashMap<String, Mask> = HashMap::new();

    let mut text_state = TextState::default();
    let mut in_text = false;
    let mut in_tspan = false;

    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"svg" | b"defs" | b"style" | b"clipPath" => {
                        // 已在 pass1 处理
                    }
                    b"g" => {
                        let parent = stack.last().unwrap().clone();
                        let mut ctx = parent.clone();
                        let amap = extract_attrs(&e.attributes());
                        if let Some(c) = attr_get(&amap, "class") {
                            apply_classes(&mut ctx, c, css);
                        }
                        apply_attrs(&mut ctx, &amap);
                        if let Some(t) = attr_get(&amap, "transform") {
                            ctx.transform = parent.transform.pre_concat(parse_transform(t));
                        }
                        stack.push(ctx);
                    }
                    b"text" => {
                        let parent = stack.last().unwrap().clone();
                        let mut ctx = parent.clone();
                        let amap = extract_attrs(&e.attributes());
                        if let Some(c) = attr_get(&amap, "class") {
                            apply_classes(&mut ctx, c, css);
                        }
                        apply_attrs(&mut ctx, &amap);
                        let text_transform = if let Some(t) = attr_get(&amap, "transform") {
                            parent.transform.pre_concat(parse_transform(t))
                        } else {
                            parent.transform
                        };
                        text_state = TextState {
                            active: true,
                            transform: text_transform,
                            font_family: ctx.font_family.clone(),
                            font_size: ctx.font_size,
                            font_style: ctx.font_style.clone(),
                            font_weight: ctx.font_weight,
                            fill: ctx.fill,
                            opacity: ctx.opacity * ctx.fill_opacity,
                            tspan_x: None,
                            tspan_y: None,
                            text_buf: String::new(),
                        };
                        in_text = true;
                    }
                    b"tspan" => {
                        if in_text {
                            let amap = extract_attrs(&e.attributes());
                            in_tspan = true;
                            text_state.tspan_x = attr_get_f32(&amap, "x");
                            text_state.tspan_y = attr_get_f32(&amap, "y");
                            if let Some(s) = attr_get(&amap, "font-family") {
                                text_state.font_family = s.trim_matches('"').to_string();
                            }
                            if let Some(s) = attr_get_f32(&amap, "font-size") {
                                text_state.font_size = s;
                            }
                            if let Some(s) = attr_get(&amap, "font-style") {
                                text_state.font_style = s.to_string();
                            }
                            if let Some(s) = attr_get(&amap, "font-weight") {
                                if let Ok(w) = s.trim().parse::<u16>() {
                                    text_state.font_weight = w;
                                } else if s.trim().eq_ignore_ascii_case("bold") {
                                    text_state.font_weight = 700;
                                }
                            }
                            if let Some(s) = attr_get(&amap, "fill") {
                                text_state.fill = parse_css_color(s);
                            }
                            text_state.text_buf.clear();
                        }
                    }
                    b"rect" => {
                        let amap = extract_attrs(&e.attributes());
                        render_rect_event(
                            pixmap,
                            &amap,
                            stack.last().unwrap(),
                            clips,
                            &mut clip_cache,
                            css,
                            scale_transform,
                            width,
                            height,
                        );
                    }
                    b"path" => {
                        let amap = extract_attrs(&e.attributes());
                        render_path_event(
                            pixmap,
                            &amap,
                            stack.last().unwrap(),
                            clips,
                            &mut clip_cache,
                            css,
                            scale_transform,
                            width,
                            height,
                        );
                    }
                    b"circle" | b"ellipse" | b"line" | b"polyline" | b"polygon" => {
                        let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");
                        let amap = extract_attrs(&e.attributes());
                        render_shape_event(
                            pixmap,
                            &amap,
                            tag,
                            stack.last().unwrap(),
                            clips,
                            &mut clip_cache,
                            css,
                            scale_transform,
                            width,
                            height,
                        );
                    }
                    b"image" | b"use" | b"linearGradient" | b"radialGradient" | b"filter"
                    | b"mask" | b"pattern" => {
                        // 静默跳过
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"rect" => {
                        let amap = extract_attrs(&e.attributes());
                        render_rect_event(
                            pixmap,
                            &amap,
                            stack.last().unwrap(),
                            clips,
                            &mut clip_cache,
                            css,
                            scale_transform,
                            width,
                            height,
                        );
                    }
                    b"path" => {
                        let amap = extract_attrs(&e.attributes());
                        render_path_event(
                            pixmap,
                            &amap,
                            stack.last().unwrap(),
                            clips,
                            &mut clip_cache,
                            css,
                            scale_transform,
                            width,
                            height,
                        );
                    }
                    b"circle" | b"ellipse" | b"line" | b"polyline" | b"polygon" => {
                        let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");
                        let amap = extract_attrs(&e.attributes());
                        render_shape_event(
                            pixmap,
                            &amap,
                            tag,
                            stack.last().unwrap(),
                            clips,
                            &mut clip_cache,
                            css,
                            scale_transform,
                            width,
                            height,
                        );
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"g" => {
                        stack.pop();
                    }
                    b"text" => {
                        // 渲染无 tspan 的残余文本
                        if in_text && !text_state.text_buf.is_empty() && !in_tspan {
                            render_text_state(pixmap, &text_state, font, scale_transform);
                        }
                        in_text = false;
                        text_state.active = false;
                    }
                    b"tspan" => {
                        if in_tspan {
                            render_text_state(pixmap, &text_state, font, scale_transform);
                            in_tspan = false;
                            text_state.text_buf.clear();
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    if in_tspan {
                        text_state.text_buf.push_str(&s);
                    } else if in_text && !in_tspan {
                        text_state.text_buf.push_str(&s);
                    }
                }
            }
            Ok(Event::CData(c)) => {
                if in_tspan {
                    text_state
                        .text_buf
                        .push_str(std::str::from_utf8(c.as_ref()).unwrap_or(""));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML 解析错误: {}", e)),
            _ => {}
        }
    }
    Ok(())
}

fn render_text_state(
    pixmap: &mut Pixmap,
    ts: &TextState,
    font: Option<&ab_glyph::FontVec>,
    scale_transform: Transform,
) {
    let font = match font {
        Some(f) => f,
        None => return,
    };
    let color = match ts.fill {
        Some(c) => c,
        None => return,
    };
    let x = ts.tspan_x.unwrap_or(0.0);
    let y = ts.tspan_y.unwrap_or(ts.font_size);
    let text = ts.text_buf.trim();
    if text.is_empty() {
        return;
    }
    // 合并 scale_transform：用户空间 transform 先应用，再缩放到 pixmap。
    // 顺序必须是 scale_transform.pre_concat(ts.transform) = scale × user，
    // 否则高 DPI 时用户空间的平移被 scale 放大，文字偏到左上角。
    let full = scale_transform.pre_concat(ts.transform);
    render_text(
        pixmap,
        font,
        text,
        x,
        y,
        ts.font_size,
        color,
        ts.opacity,
        full,
        &ts.font_style,
    );
}

fn render_rect_event(
    pixmap: &mut Pixmap,
    amap: &HashMap<String, String>,
    ctx: &RenderCtx,
    clips: &HashMap<String, Vec<ClipRect>>,
    clip_cache: &mut HashMap<String, Mask>,
    css: &HashMap<String, CssClass>,
    scale_transform: Transform,
    width: u32,
    height: u32,
) {
    if ctx.display_none {
        return;
    }
    let mut local_ctx = ctx.clone();
    if let Some(c) = attr_get(amap, "class") {
        apply_classes(&mut local_ctx, c, css);
    }
    apply_attrs(&mut local_ctx, amap);

    let x = attr_get_f32(amap, "x").unwrap_or(0.0);
    let y = attr_get_f32(amap, "y").unwrap_or(0.0);
    let w = attr_get_f32(amap, "width").unwrap_or(0.0);
    let h = attr_get_f32(amap, "height").unwrap_or(0.0);
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let elem_transform = if let Some(t) = attr_get(amap, "transform") {
        local_ctx.transform.pre_concat(parse_transform(t))
    } else {
        local_ctx.transform
    };
    let full_transform = scale_transform.pre_concat(elem_transform);

    let clip_mask = get_clip_mask(&local_ctx, clips, clip_cache, width, height);

    // fill（fill:none 时跳过填充，但仍绘制 stroke）
    if let Some(color) = local_ctx.fill {
        let opacity = local_ctx.opacity * local_ctx.fill_opacity;
        if opacity > 0.0 {
            let rect = match Rect::from_xywh(x, y, w, h) {
                Some(r) => r,
                None => return,
            };
            let mut paint = Paint::default();
            paint.set_color_rgba8(
                color.0,
                color.1,
                color.2,
                (opacity * 255.0).clamp(0.0, 255.0) as u8,
            );
            paint.anti_alias = true;
            pixmap.fill_rect(rect, &paint, full_transform, clip_mask);
        }
    }

    // stroke
    if let Some(sc) = local_ctx.stroke {
        if local_ctx.stroke_width > 0.0 {
            let rect = match Rect::from_xywh(x, y, w, h) {
                Some(r) => r,
                None => return,
            };
            let mut stroke_paint = Paint::default();
            stroke_paint.set_color_rgba8(
                sc.0,
                sc.1,
                sc.2,
                (local_ctx.stroke_opacity * local_ctx.opacity * 255.0).clamp(0.0, 255.0) as u8,
            );
            stroke_paint.anti_alias = true;
            let mut sp = tiny_skia::Stroke::default();
            // sp.width 保持用户单位；full_transform（含 DPI 缩放）会在 stroke_path 内部
            // 把描边路径连同宽度一起变换到 pixmap 像素空间。
            sp.width = local_ctx.stroke_width;
            let path = PathBuilder::from_rect(rect);
            pixmap.stroke_path(&path, &stroke_paint, &sp, full_transform, clip_mask);
        }
    }
}

fn render_path_event(
    pixmap: &mut Pixmap,
    amap: &HashMap<String, String>,
    ctx: &RenderCtx,
    clips: &HashMap<String, Vec<ClipRect>>,
    clip_cache: &mut HashMap<String, Mask>,
    css: &HashMap<String, CssClass>,
    scale_transform: Transform,
    width: u32,
    height: u32,
) {
    if ctx.display_none {
        return;
    }
    let mut local_ctx = ctx.clone();
    if let Some(c) = attr_get(amap, "class") {
        apply_classes(&mut local_ctx, c, css);
    }
    apply_attrs(&mut local_ctx, amap);

    let d = match attr_get(amap, "d") {
        Some(d) => d,
        None => return,
    };
    let path = match build_path(d) {
        Ok(p) => p,
        Err(_) => return,
    };

    let elem_transform = if let Some(t) = attr_get(amap, "transform") {
        local_ctx.transform.pre_concat(parse_transform(t))
    } else {
        local_ctx.transform
    };
    let full_transform = scale_transform.pre_concat(elem_transform);

    let clip_mask = get_clip_mask(&local_ctx, clips, clip_cache, width, height);

    // fill（fill:none 时跳过填充，但仍绘制 stroke）
    if let Some(color) = local_ctx.fill {
        let opacity = local_ctx.opacity * local_ctx.fill_opacity;
        if opacity > 0.0 {
            let mut paint = Paint::default();
            paint.set_color_rgba8(
                color.0,
                color.1,
                color.2,
                (opacity * 255.0).clamp(0.0, 255.0) as u8,
            );
            paint.anti_alias = true;
            pixmap.fill_path(&path, &paint, FillRule::Winding, full_transform, clip_mask);
        }
    }

    // stroke
    if let Some(sc) = local_ctx.stroke {
        if local_ctx.stroke_width > 0.0 {
            let mut stroke_paint = Paint::default();
            stroke_paint.set_color_rgba8(
                sc.0,
                sc.1,
                sc.2,
                (local_ctx.stroke_opacity * local_ctx.opacity * 255.0).clamp(0.0, 255.0) as u8,
            );
            stroke_paint.anti_alias = true;
            let mut sp = tiny_skia::Stroke::default();
            // sp.width 保持用户单位；full_transform（含 DPI 缩放）会在 stroke_path 内部
            // 把描边路径连同宽度一起变换到 pixmap 像素空间。
            sp.width = local_ctx.stroke_width;
            pixmap.stroke_path(&path, &stroke_paint, &sp, full_transform, clip_mask);
        }
    }
}

/// 将 <line>/<polyline>/<polygon>/<circle>/<ellipse> 转为 path 的 d 属性
fn shape_to_d(amap: &HashMap<String, String>, tag: &str) -> Option<String> {
    match tag {
        "line" => {
            let x1 = attr_get_f32(amap, "x1").unwrap_or(0.0);
            let y1 = attr_get_f32(amap, "y1").unwrap_or(0.0);
            let x2 = attr_get_f32(amap, "x2").unwrap_or(0.0);
            let y2 = attr_get_f32(amap, "y2").unwrap_or(0.0);
            Some(format!("M {} {} L {} {}", x1, y1, x2, y2))
        }
        "polyline" => {
            let pts = attr_get(amap, "points")?;
            let nums: Vec<f32> = pts
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            if nums.len() < 2 || nums.len() % 2 != 0 {
                return None;
            }
            let mut d = format!("M {} {}", nums[0], nums[1]);
            for chunk in nums[2..].chunks(2) {
                d.push_str(&format!(" L {} {}", chunk[0], chunk[1]));
            }
            Some(d)
        }
        "polygon" => {
            let pts = attr_get(amap, "points")?;
            let nums: Vec<f32> = pts
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            if nums.len() < 2 || nums.len() % 2 != 0 {
                return None;
            }
            let mut d = format!("M {} {}", nums[0], nums[1]);
            for chunk in nums[2..].chunks(2) {
                d.push_str(&format!(" L {} {}", chunk[0], chunk[1]));
            }
            d.push_str(" Z");
            Some(d)
        }
        "circle" => {
            let cx = attr_get_f32(amap, "cx").unwrap_or(0.0);
            let cy = attr_get_f32(amap, "cy").unwrap_or(0.0);
            let r = attr_get_f32(amap, "r").unwrap_or(0.0);
            if r <= 0.0 {
                return None;
            }
            // 用 4 段三次贝塞尔近似圆
            let k = r * 0.5522847498;
            Some(format!(
                "M {} {} C {} {} {} {} {} {} C {} {} {} {} {} {} C {} {} {} {} {} {} C {} {} {} {} {} {} Z",
                cx - r, cy,
                cx - r, cy - k, cx - k, cy - r, cx, cy - r,
                cx + k, cy - r, cx + r, cy - k, cx + r, cy,
                cx + r, cy + k, cx + k, cy + r, cx, cy + r,
                cx - k, cy + r, cx - r, cy + k, cx - r, cy,
            ))
        }
        "ellipse" => {
            let cx = attr_get_f32(amap, "cx").unwrap_or(0.0);
            let cy = attr_get_f32(amap, "cy").unwrap_or(0.0);
            let rx = attr_get_f32(amap, "rx").unwrap_or(0.0);
            let ry = attr_get_f32(amap, "ry").unwrap_or(0.0);
            if rx <= 0.0 || ry <= 0.0 {
                return None;
            }
            let kx = rx * 0.5522847498;
            let ky = ry * 0.5522847498;
            Some(format!(
                "M {} {} C {} {} {} {} {} {} C {} {} {} {} {} {} C {} {} {} {} {} {} C {} {} {} {} {} {} Z",
                cx - rx, cy,
                cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry,
                cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy,
                cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry,
                cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy,
            ))
        }
        _ => None,
    }
}

/// 渲染 line/polyline/polygon/circle/ellipse（转为 path 复用 render_path_event）
fn render_shape_event(
    pixmap: &mut Pixmap,
    amap: &HashMap<String, String>,
    tag: &str,
    ctx: &RenderCtx,
    clips: &HashMap<String, Vec<ClipRect>>,
    clip_cache: &mut HashMap<String, Mask>,
    css: &HashMap<String, CssClass>,
    scale_transform: Transform,
    width: u32,
    height: u32,
) {
    let d = match shape_to_d(amap, tag) {
        Some(d) => d,
        None => return,
    };
    // 构造带 d 属性的 amap 副本
    let mut dmap = amap.clone();
    dmap.insert("d".to_string(), d);
    render_path_event(
        pixmap,
        &dmap,
        ctx,
        clips,
        clip_cache,
        css,
        scale_transform,
        width,
        height,
    );
}

// ============ napi 导出 ============

/// 混合流式 SVG 转栅格（针对超大 SVG，usvg 无法处理时使用）
///
/// 支持特性：rect / path（d 属性）/ text（ab_glyph）/ g（样式栈）/ clipPath（矩形）
/// 不支持：渐变 / 滤镜 / pattern / mask / image / 非矩形 clipPath（静默跳过 + 警告）
#[napi]
pub fn convert_svg_fast(
    input_path: String,
    output_path: String,
    options: Option<SvgConvertOptions>,
) -> SvgConvertResult {
    convert_svg_fast_inner(&input_path, &output_path, options.unwrap_or_default())
}

fn convert_svg_fast_inner(
    input_path: &str,
    output_path: &str,
    opts: SvgConvertOptions,
) -> SvgConvertResult {
    let result = convert_svg_fast_impl(input_path, output_path, &opts);
    match result {
        Ok(r) => r,
        Err(e) => SvgConvertResult {
            ok: false,
            width: 0,
            height: 0,
            bytes_written: 0,
            format: opts
                .format
                .clone()
                .unwrap_or_else(|| "png".to_string())
                .to_lowercase(),
            dpi: opts.dpi.unwrap_or(96.0),
            error: Some(e),
        },
    }
}

fn convert_svg_fast_impl(
    input_path: &str,
    output_path: &str,
    opts: &SvgConvertOptions,
) -> Result<SvgConvertResult, String> {
    // 第一遍：收集 CSS / clipPath / 尺寸
    let p1 = pass1(input_path)?;

    // 计算目标尺寸
    let (tw, th, sx, sy) =
        compute_target_size(p1.svg_w_px, p1.svg_h_px, opts, p1.phys_w_in, p1.phys_h_in);

    if tw == 0 || th == 0 {
        return Err(format!("目标尺寸无效: {}x{}", tw, th));
    }

    // 创建 pixmap
    let mut pixmap = Pixmap::new(tw, th)
        .ok_or_else(|| format!("无法创建 {}x{} pixmap（内存不足？）", tw, th))?;

    // 填充背景。默认不透明白色（避免半透明内容在透明背景上渲染后灰蒙蒙：
    // 透明背景 dst=(0,0,0,0)，半透明 src 叠加后 RGB 被乘以 alpha 变暗，
    // flatten_to_rgb 合成白底时又多乘一次 alpha）。
    // 如需透明背景，显式传 background: "#00000000"
    let bg = opts
        .background
        .as_ref()
        .and_then(|s| parse_hex_color(s))
        .unwrap_or((255, 255, 255, 255));
    pixmap.fill(tiny_skia::Color::from_rgba8(bg.0, bg.1, bg.2, bg.3));

    // 加载字体
    let font = load_font();

    // 第二遍：渲染
    let mut warnings = Vec::new();
    pass2(
        input_path,
        &mut pixmap,
        &p1.css,
        &p1.clips,
        font.as_ref(),
        sx as f32,
        sy as f32,
        &mut warnings,
    )?;

    // 编码输出
    let format = opts
        .format
        .clone()
        .unwrap_or_else(|| "png".to_string())
        .to_lowercase();
    let quality = opts.quality.unwrap_or(92);
    let dpi = opts.dpi.unwrap_or(96.0);
    let bytes = encode_pixmap(&pixmap, &format, quality, dpi)?;

    // 写文件
    let file = File::create(output_path).map_err(|e| format!("创建输出文件失败: {}", e))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(&bytes)
        .map_err(|e| format!("写入失败: {}", e))?;
    writer.flush().map_err(|e| format!("刷新失败: {}", e))?;

    let warning_msg = if warnings.is_empty() {
        None
    } else if warnings.len() > 10 {
        Some(format!(
            "渲染完成，但跳过了部分不支持的特性（{} 类，如: {}）",
            warnings.len(),
            warnings[0]
        ))
    } else {
        Some(format!(
            "渲染完成，但跳过了部分特性: {}",
            warnings.join("; ")
        ))
    };

    Ok(SvgConvertResult {
        ok: true,
        width: tw,
        height: th,
        bytes_written: bytes.len() as i64,
        format,
        dpi,
        error: warning_msg,
    })
}
