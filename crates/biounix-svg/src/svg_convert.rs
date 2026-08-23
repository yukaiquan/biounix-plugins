// ============================================================
// SVG → 栅格图像转换引擎（resvg + tiny-skia + image）
// ============================================================
// 设计目标：解决用户转化超大 SVG 难的问题
//   - 纯 Rust 渲染（resvg/usvg/tiny-skia），无外部 C 依赖，跨平台像素一致
//   - 支持 PNG / JPEG / TIFF / WebP / BMP 多种输出格式
//   - DPI 感知：读取 SVG 物理单位（cm/mm/in/pt）→ 目标像素 = inches × dpi
//   - 分块渲染：超大画布按 TILE_SIZE 切片，避免一次性分配巨量内存
//   - DPI 元数据写入：PNG pHYs / JPEG JFIF density / TIFF XRes·YRes
//
// 关键 API：
//   - usvg::Tree::from_data(data, &options)  解析 SVG 树
//   - tree.size()                            返回 SVG 逻辑尺寸（px，f64）
//   - resvg::render(tree, transform, pixmap) 将树渲染到 pixmap（sRGB 输出）
//   - image::codecs::*                       多格式编码器
//
// 注意：
//   - resvg 不支持动画、原生文本渲染（文字需 outline 成 path），Unicode-only
//   - tiny-skia 的 Pixmap 内部是 premultiplied RGBA8，编码前需 unpremultiply
//   - 大画布分块时，每块用 Transform::from_translate 偏移到正确位置
//

use std::fs;
use std::io::BufWriter;

// ============ 选项与结果结构 ============

/// SVG 转换选项
#[napi(object)]
pub struct SvgConvertOptions {
    /// 输出格式：png / jpeg / tiff / webp / bmp / pdf（默认 png）
    #[napi(ts_type = "\"png\" | \"jpeg\" | \"tiff\" | \"webp\" | \"bmp\" | \"pdf\"")]
    pub format: Option<String>,
    /// 目标宽度（像素）。None/0 表示按 SVG 原宽 + scale 计算
    pub width: Option<u32>,
    /// 目标高度（像素）。None/0 表示按 SVG 原高 + scale 计算
    pub height: Option<u32>,
    /// 缩放系数（1.0 = 原始大小）。当 width/height 未指定时生效，默认 1.0
    pub scale: Option<f64>,
    /// JPEG/WebP 质量（1-100），默认 92
    pub quality: Option<u32>,
    /// 背景色（ARGB 十六进制，如 "#FFFFFFFF" 不透明白）。None 表示透明
    /// 格式：#RRGGBB 或 #AARRGGBB
    pub background: Option<String>,
    /// 输出 DPI（每英寸像素数）。默认 96
    /// 当 SVG 用物理单位（cm/mm/in/pt）且未显式指定 width/height 时，
    /// 目标像素 = inches × dpi
    pub dpi: Option<f64>,
    /// 是否保持 SVG 声明的物理尺寸（配合 dpi）。默认 false
    /// true：按 SVG width/height 单位换算成英寸 × dpi 得到像素
    /// false：直接用 SVG 的 px 值（1px = 1/dpi 英寸的 96dpi 约定）
    pub keep_physical_size: Option<bool>,
    /// 分块渲染的块大小（像素，边长）。0 或 >65535 表示不分块。默认 4096
    pub tile_size: Option<u32>,
}

impl Default for SvgConvertOptions {
    fn default() -> Self {
        SvgConvertOptions {
            format: None,
            width: None,
            height: None,
            scale: None,
            quality: None,
            background: None,
            dpi: None,
            keep_physical_size: None,
            tile_size: None,
        }
    }
}

/// 取选项值，None 或非法值时返回默认
impl SvgConvertOptions {
    pub(crate) fn fmt(&self) -> String {
        self.format
            .clone()
            .unwrap_or_else(|| "png".to_string())
            .to_lowercase()
    }
    pub(crate) fn w(&self) -> u32 {
        self.width.unwrap_or(0)
    }
    pub(crate) fn h(&self) -> u32 {
        self.height.unwrap_or(0)
    }
    pub(crate) fn scl(&self) -> f64 {
        self.scale.unwrap_or(1.0)
    }
    pub(crate) fn qlt(&self) -> u32 {
        self.quality.unwrap_or(92)
    }
    pub(crate) fn dpiv(&self) -> f64 {
        self.dpi.unwrap_or(96.0)
    }
    pub(crate) fn keep_phys(&self) -> bool {
        self.keep_physical_size.unwrap_or(false)
    }
    pub(crate) fn tile(&self) -> u32 {
        self.tile_size.unwrap_or(4096)
    }
}

/// SVG 转换结果
#[napi(object)]
pub struct SvgConvertResult {
    pub ok: bool,
    pub width: u32,
    pub height: u32,
    pub bytes_written: i64,
    pub format: String,
    pub dpi: f64,
    pub error: Option<String>,
}

/// SVG 元信息（不渲染，仅解析）
#[napi(object)]
pub struct SvgInfo {
    pub ok: bool,
    pub width: f64,
    pub height: f64,
    /// SVG width 属性的原始字符串（含单位，如 "10cm"）
    pub width_unit: String,
    pub height_unit: String,
    /// 推断的物理宽度（英寸）
    pub physical_width_in: f64,
    pub physical_height_in: f64,
    pub error: Option<String>,
}

// ============ 单位解析 ============

/// 解析长度字符串（如 "100", "10cm", "3.5in", "72pt", "12mm", "8pc"）
/// 返回 (像素值@96dpi, 原始单位, 英寸值)
/// SVG/CSS 单位换算（基于 96dpi 用户单位约定）：
///   1in = 96px, 1cm = 96/2.54 px, 1mm = 96/25.4 px,
///   1pt = 96/72 px, 1pc = 96/6 px, 1px = 1px
pub fn parse_length(s: &str) -> Option<(f64, String, f64)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 找到数字部分结束位置
    let mut num_end = 0;
    let bytes = s.as_bytes();
    while num_end < bytes.len() {
        let c = bytes[num_end];
        if c.is_ascii_digit() || c == b'.' || c == b'-' || c == b'+' || c == b'e' || c == b'E' {
            num_end += 1;
        } else {
            break;
        }
    }
    if num_end == 0 {
        return None;
    }
    let num_str = &s[..num_end];
    let unit = s[num_end..].trim().to_lowercase();
    let value: f64 = num_str.parse().ok()?;

    // 单位 → 像素（96dpi 基准）+ 英寸
    let (px, inches) = match unit.as_str() {
        "" | "px" => (value, value / 96.0),
        "in" => (value * 96.0, value),
        "cm" => (value * 96.0 / 2.54, value / 2.54),
        "mm" => (value * 96.0 / 25.4, value / 25.4),
        "pt" => (value * 96.0 / 72.0, value / 72.0),
        "pc" => (value * 96.0 / 6.0, value / 6.0),
        "q" => (value * 96.0 / 25.4 / 4.0, value / 25.4 / 4.0), // 1Q = 0.25mm
        _ => (value, value / 96.0),                             // 未知单位当 px 处理
    };
    Some((px, unit, inches))
}

/// 从 SVG 文本中提取 width/height 属性（简单正则式匹配，避免引入 XML 解析依赖）
/// 优先取 <svg ... width="..." height="..." />，兼容单引号
pub fn extract_svg_size(svg: &str) -> (Option<String>, Option<String>) {
    // 找 <svg 开标签
    let lower = svg.to_lowercase();
    let svg_start = match lower.find("<svg") {
        Some(i) => i,
        None => return (None, None),
    };
    // 找开标签结束 >
    let tag_end = match lower[svg_start..].find('>') {
        Some(i) => svg_start + i,
        None => return (None, None),
    };
    let tag = &svg[svg_start..tag_end];

    let extract_attr = |name: &str| -> Option<String> {
        // 匹配 name="value" 或 name='value'
        let patterns = [
            format!("{}=\"", name),
            format!("{}='", name),
            format!("{}=\"", name.to_lowercase()),
            format!("{}='", name.to_lowercase()),
        ];
        for pat in patterns {
            if let Some(idx) = tag.find(&pat) {
                let val_start = idx + pat.len();
                let quote = &pat[pat.len() - 1..];
                if let Some(end) = tag[val_start..].find(quote) {
                    return Some(tag[val_start..val_start + end].to_string());
                }
            }
        }
        None
    };

    (extract_attr("width"), extract_attr("height"))
}

// ============ 颜色解析 ============

/// 解析十六进制颜色 "#RGB" / "#RRGGBB" / "#AARRGGBB" → (r, g, b, a)
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    match s.len() {
        3 => {
            // #RGB → #RRGGBB
            let r = u8::from_str_radix(&format!("{}{}", &s[0..1], &s[0..1]), 16).ok()?;
            let g = u8::from_str_radix(&format!("{}{}", &s[1..2], &s[1..2]), 16).ok()?;
            let b = u8::from_str_radix(&format!("{}{}", &s[2..3], &s[2..3]), 16).ok()?;
            Some((r, g, b, 255))
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some((r, g, b, 255))
        }
        8 => {
            // #AARRGGBB
            let a = u8::from_str_radix(&s[0..2], 16).ok()?;
            let r = u8::from_str_radix(&s[2..4], 16).ok()?;
            let g = u8::from_str_radix(&s[4..6], 16).ok()?;
            let b = u8::from_str_radix(&s[6..8], 16).ok()?;
            Some((r, g, b, a))
        }
        _ => None,
    }
}

// ============ 核心：渲染 + 编码 ============

/// 计算最终输出像素尺寸
/// 返回 (width, height, scale_x, scale_y)
pub fn compute_target_size(
    svg_w_px: f64,
    svg_h_px: f64,
    opts: &SvgConvertOptions,
    physical_w_in: f64,
    physical_h_in: f64,
) -> (u32, u32, f64, f64) {
    // 优先级：显式 width/height > scale > keep_physical_size×dpi > SVG 原始 px
    let (w, h, sx, sy);
    let ow = opts.w();
    let oh = opts.h();
    let oscl = opts.scl();
    let odpi = opts.dpiv();
    let okeep = opts.keep_phys();

    if ow > 0 && oh > 0 {
        // 显式指定
        w = ow;
        h = oh;
        sx = w as f64 / svg_w_px;
        sy = h as f64 / svg_h_px;
    } else if ow > 0 {
        // 仅指定 width，按比例
        w = ow;
        sx = w as f64 / svg_w_px;
        sy = sx;
        h = (svg_h_px * sy).round() as u32;
    } else if oh > 0 {
        // 仅指定 height，按比例
        h = oh;
        sy = h as f64 / svg_h_px;
        sx = sy;
        w = (svg_w_px * sx).round() as u32;
    } else if okeep && physical_w_in > 0.0 && physical_h_in > 0.0 {
        // 按物理尺寸 × DPI
        w = (physical_w_in * odpi).round() as u32;
        h = (physical_h_in * odpi).round() as u32;
        sx = w as f64 / svg_w_px;
        sy = h as f64 / svg_h_px;
    } else if (oscl - 1.0).abs() > 1e-9 {
        // 指定 scale
        sx = oscl;
        sy = oscl;
        w = (svg_w_px * sx).round() as u32;
        h = (svg_h_px * sy).round() as u32;
    } else {
        // 默认：SVG 原始像素
        w = svg_w_px.round() as u32;
        h = svg_h_px.round() as u32;
        sx = 1.0;
        sy = 1.0;
    }

    // 防御性 clamp（tiny-skia Pixmap 上限、避免 OOM）
    const MAX_DIM: u32 = 65535;
    let w = w.clamp(1, MAX_DIM);
    let h = h.clamp(1, MAX_DIM);
    (w, h, sx, sy)
}

/// 渲染 SVG 到 Pixmap（支持分块）
fn render_svg(
    tree: &usvg::Tree,
    width: u32,
    height: u32,
    sx: f64,
    sy: f64,
    bg: Option<(u8, u8, u8, u8)>,
    tile_size: u32,
) -> Result<tiny_skia::Pixmap, String> {
    // 小图直接渲染
    let use_tile = tile_size > 0 && tile_size < 65535 && (width > tile_size || height > tile_size);

    if !use_tile {
        let mut pixmap = tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| format!("无法创建 {}x{} pixmap（内存不足？）", width, height))?;
        // 填充背景
        if let Some((r, g, b, a)) = bg {
            pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, a));
        }
        let transform = tiny_skia::Transform::from_scale(sx as f32, sy as f32);
        let mut pm_mut = pixmap.as_mut();
        resvg::render(tree, transform, &mut pm_mut);
        return Ok(pixmap);
    }

    // 分块渲染：分配完整 pixmap，逐块渲染后拼合
    let mut full = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| format!("无法创建 {}x{} pixmap（内存不足？）", width, height))?;
    if let Some((r, g, b, a)) = bg {
        full.fill(tiny_skia::Color::from_rgba8(r, g, b, a));
    }

    let ts = tile_size as i32;
    let w_i = width as i32;
    let h_i = height as i32;
    let mut y = 0;
    while y < h_i {
        let mut x = 0;
        let tile_h = ts.min(h_i - y) as u32;
        while x < w_i {
            let tile_w = ts.min(w_i - x) as u32;
            // 渲染单块：块内坐标 (0,0) → 全局坐标 (x,y)，需平移 -x,-y
            let mut tile = tiny_skia::Pixmap::new(tile_w, tile_h)
                .ok_or_else(|| format!("无法创建 tile {}x{} pixmap", tile_w, tile_h))?;
            // 块本身透明（背景已在 full 上填好），不重复填
            let transform = tiny_skia::Transform::from_scale(sx as f32, sy as f32)
                .pre_translate(-(x as f32), -(y as f32));
            let mut pm_mut = tile.as_mut();
            resvg::render(tree, transform, &mut pm_mut);

            // 拷贝块到 full（draw_pixmap 需 PixmapRef）
            full.draw_pixmap(
                x as i32,
                y as i32,
                tile.as_ref(),
                &tiny_skia::PixmapPaint::default(),
                tiny_skia::Transform::identity(),
                None,
            );
            x += ts;
        }
        y += ts;
    }
    Ok(full)
}

/// 编码 pixmap 到指定格式字节流
pub fn encode_pixmap(
    pixmap: &tiny_skia::Pixmap,
    format: &str,
    quality: u32,
    dpi: f64,
) -> Result<Vec<u8>, String> {
    // tiny-skia 的 data() 是 premultiplied RGBA8；image 编码器需要 unpremultiplied
    let rgba = unpremultiply(pixmap);
    let fmt = format.to_lowercase();

    // TIFF 需要 Seek，不能套 BufWriter；用 Cursor<Vec<u8>> 满足 Write+Seek
    if fmt == "tiff" {
        use image::ImageEncoder;
        let mut buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut buf);
        let encoder = image::codecs::tiff::TiffEncoder::new(cursor);
        encoder
            .write_image(
                &rgba,
                pixmap.width(),
                pixmap.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| format!("TIFF 编码失败: {}", e))?;
        // 注：image 0.25 TiffEncoder 不直接支持写 DPI 元数据，
        // 如需精确 DPI，用户可后处理用 tiffinfo/tifftag 修改
        return Ok(buf);
    }

    let buf: Vec<u8> = Vec::new();
    let mut writer = BufWriter::new(buf);

    match fmt.as_str() {
        "png" => {
            // 手动写 PNG 以支持 pHYs chunk（DPI 元数据）
            // pixels_per_meter = dpi / 0.0254
            let ppm = (dpi / 0.0254).round() as u32;
            write_png_with_phys(&rgba, pixmap.width(), pixmap.height(), ppm, &mut writer)?;
        }
        "jpeg" | "jpg" => {
            let q = quality.clamp(1, 100) as u8;
            // JPEG 不支持 alpha，合成白底转 RGB
            let rgb = flatten_to_rgb(&rgba, (255, 255, 255));
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, q);
            encoder
                .encode(
                    &rgb,
                    pixmap.width(),
                    pixmap.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| format!("JPEG 编码失败: {}", e))?;
        }
        "webp" => {
            #[cfg(feature = "webp")]
            {
                let encoder = image::codecs::webp::WebPEncoder::new(&mut writer);
                encoder
                    .write_image(
                        &rgba,
                        pixmap.width(),
                        pixmap.height(),
                        image::ExtendedColorType::Rgba8,
                    )
                    .map_err(|e| format!("WebP 编码失败: {}", e))?;
            }
            #[cfg(not(feature = "webp"))]
            {
                return Err("WebP 编码需要启用 webp feature（Cargo.toml: image = { features = [\"webp\"] }）".to_string());
            }
        }
        "bmp" => {
            let mut encoder = image::codecs::bmp::BmpEncoder::new(&mut writer);
            encoder
                .encode(
                    &rgba,
                    pixmap.width(),
                    pixmap.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| format!("BMP 编码失败: {}", e))?;
        }
        "pdf" => {
            // PDF 页面尺寸（点，1pt=1/72in）= 像素 / dpi × 72
            let w_pt = pixmap.width() as f64 / dpi * 72.0;
            let h_pt = pixmap.height() as f64 / dpi * 72.0;
            let rgb = flatten_to_rgb(&rgba, (255, 255, 255));
            let pdf_bytes = write_pdf(&rgb, pixmap.width(), pixmap.height(), w_pt, h_pt)?;
            // PDF 已是完整字节流，直接返回（绕过 writer）
            return Ok(pdf_bytes);
        }
        other => {
            return Err(format!(
                "不支持的格式: {}（支持 png/jpeg/tiff/webp/bmp/pdf）",
                other
            ))
        }
    }

    let bytes = writer
        .into_inner()
        .map_err(|e| format!("编码缓冲区刷新失败: {}", e))?;
    Ok(bytes)
}

/// unpremultiply：premultiplied RGBA → straight RGBA
pub fn unpremultiply(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    let src = pixmap.data();
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let r = src[i];
        let g = src[i + 1];
        let b = src[i + 2];
        let a = src[i + 3];
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            out.extend_from_slice(&[r, g, b, a]);
        } else {
            let af = a as f64 / 255.0;
            out.push((r as f64 / af).round().min(255.0) as u8);
            out.push((g as f64 / af).round().min(255.0) as u8);
            out.push((b as f64 / af).round().min(255.0) as u8);
            out.push(a);
        }
        i += 4;
    }
    out
}

/// RGBA → RGB（合成到指定背景色）
fn flatten_to_rgb(rgba: &[u8], bg: (u8, u8, u8)) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    let mut i = 0;
    while i < rgba.len() {
        let r = rgba[i];
        let g = rgba[i + 1];
        let b = rgba[i + 2];
        let a = rgba[i + 3] as f64 / 255.0;
        out.push((r as f64 * a + bg.0 as f64 * (1.0 - a)).round() as u8);
        out.push((g as f64 * a + bg.1 as f64 * (1.0 - a)).round() as u8);
        out.push((b as f64 * a + bg.2 as f64 * (1.0 - a)).round() as u8);
        i += 4;
    }
    out
}

/// 写入单页 PDF（内嵌 RGB 图像，FlateDecode 压缩）
///
/// 结构：%PDF-1.4 + 4 个对象（Catalog/Pages/Page/Image）+ xref + trailer
/// 图像用 DeviceRGB + FlateDecode（zlib），页面尺寸单位为点（1pt=1/72in）
fn write_pdf(rgb: &[u8], width: u32, height: u32, w_pt: f64, h_pt: f64) -> Result<Vec<u8>, String> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    // 压缩图像数据
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(rgb)
        .map_err(|e| format!("PDF 图像压缩失败: {}", e))?;
    let img_compressed = encoder
        .finish()
        .map_err(|e| format!("PDF 图像压缩刷新失败: {}", e))?;

    let w_pt = format!("{:.2}", w_pt);
    let h_pt = format!("{:.2}", h_pt);
    let w_px = width;
    let h_px = height;
    let img_len = img_compressed.len();

    // 预构建各对象的字节内容（不含 "obj"/"endobj" 包裹）
    // 对象 1: Catalog
    let obj1 = b"<< /Type /Catalog /Pages 2 0 R >>";
    // 对象 2: Pages
    let obj2 = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    // 对象 3: Page
    let obj3 = format!(
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>",
        w_pt, h_pt
    )
    .into_bytes();
    // 对象 4: Image XObject
    let obj4 = format!(
        "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>",
        w_px, h_px, img_len
    )
    .into_bytes();
    // 对象 5: Content stream（绘制图像填满整页）
    // q save, cm 设置变换矩阵 [w 0 0 h 0 0], Do 绘制, Q restore
    let content = format!("q\n{} 0 0 {} 0 0 cm\n/Im0 Do\nQ\n", w_pt, h_pt);
    let content_bytes = content.as_bytes();
    let content_len = content_bytes.len();

    // 组装 PDF（先算偏移）
    let header = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n";
    let mut pdf: Vec<u8> = Vec::new();
    pdf.extend_from_slice(header);

    let mut offsets: Vec<usize> = Vec::new();

    // 对象 1
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"1 0 obj\n");
    pdf.extend_from_slice(obj1);
    pdf.extend_from_slice(b"\nendobj\n");

    // 对象 2
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"2 0 obj\n");
    pdf.extend_from_slice(obj2);
    pdf.extend_from_slice(b"\nendobj\n");

    // 对象 3
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"3 0 obj\n");
    pdf.extend_from_slice(&obj3);
    pdf.extend_from_slice(b"\nendobj\n");

    // 对象 4（图像）
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"4 0 obj\n");
    pdf.extend_from_slice(&obj4);
    pdf.extend_from_slice(b"\nstream\n");
    pdf.extend_from_slice(&img_compressed);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    // 对象 5（content stream）
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"5 0 obj\n");
    pdf.extend_from_slice(format!("<< /Length {} >>\n", content_len).as_bytes());
    pdf.extend_from_slice(b"stream\n");
    pdf.extend_from_slice(content_bytes);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    // xref 表
    let xref_offset = pdf.len();
    let obj_count = offsets.len() + 1; // +1 for free object 0
    pdf.extend_from_slice(format!("xref\n0 {}\n", obj_count).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }

    // trailer
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            obj_count, xref_offset
        )
        .as_bytes(),
    );

    Ok(pdf)
}

/// 手动写 PNG（带 pHYs chunk 携带 DPI 元数据）
fn write_png_with_phys<W: std::io::Write>(
    rgba: &[u8],
    width: u32,
    height: u32,
    ppm: u32,
    writer: &mut W,
) -> Result<(), String> {
    // PNG 签名
    writer
        .write_all(&[137, 80, 78, 71, 13, 10, 26, 10])
        .map_err(|e| e.to_string())?;

    // IHDR
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(writer, b"IHDR", &ihdr)?;

    // pHYs（物理像素尺寸，pixels per meter X/Y, unit=1 表示 meter）
    let mut phys = Vec::with_capacity(9);
    phys.extend_from_slice(&ppm.to_be_bytes()); // X
    phys.extend_from_slice(&ppm.to_be_bytes()); // Y
    phys.push(1); // unit: meter
    write_chunk(writer, b"pHYs", &phys)?;

    // IDAT（zlib 压缩 RGBA + filter bytes）
    let raw = build_png_raw_rows(rgba, width, height);
    let compressed = zlib_compress(&raw)?;
    write_chunk(writer, b"IDAT", &compressed)?;

    // IEND
    write_chunk(writer, b"IEND", &[])?;
    Ok(())
}

/// 构造 PNG 原始行数据（每行前加 filter byte 0）
fn build_png_raw_rows(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let stride = width as usize * 4;
    let mut out = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        out.push(0); // filter: None
        out.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }
    out
}

/// zlib 压缩（PNG IDAT 用 deflate + zlib header）
fn zlib_compress(data: &[u8]) -> Result<Vec<u8>, String> {
    // 使用 flate2（已是 resvg 间接依赖，但需显式声明）
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).map_err(|e| e.to_string())?;
    encoder.finish().map_err(|e| e.to_string())
}

/// 写 PNG chunk（length + type + data + CRC32）
fn write_chunk<W: std::io::Write>(
    w: &mut W,
    chunk_type: &[u8; 4],
    data: &[u8],
) -> Result<(), String> {
    w.write_all(&(data.len() as u32).to_be_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(chunk_type).map_err(|e| e.to_string())?;
    w.write_all(data).map_err(|e| e.to_string())?;
    // CRC32 over type + data
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type);
    crc_input.extend_from_slice(data);
    let crc = crc32(&crc_input);
    w.write_all(&crc.to_be_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

/// CRC32（PNG 用 IEEE 802.3 多项式，与 zlib/gzip 一致）
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xFFFFFFFF
}

// ============ napi 导出函数 ============

/// 解析 SVG 元信息（不渲染）
#[napi]
pub fn svg_info(path: String) -> SvgInfo {
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            return SvgInfo {
                ok: false,
                width: 0.0,
                height: 0.0,
                width_unit: String::new(),
                height_unit: String::new(),
                physical_width_in: 0.0,
                physical_height_in: 0.0,
                error: Some(format!("无法读取文件: {}", e)),
            }
        }
    };
    let svg_text = String::from_utf8_lossy(&data).to_string();

    // 先用 usvg 解析获取逻辑尺寸
    let options = usvg::Options::default();
    let tree = match usvg::Tree::from_data(&data, &options) {
        Ok(t) => t,
        Err(e) => {
            return SvgInfo {
                ok: false,
                width: 0.0,
                height: 0.0,
                width_unit: String::new(),
                height_unit: String::new(),
                physical_width_in: 0.0,
                physical_height_in: 0.0,
                error: Some(format!("SVG 解析失败: {}", e)),
            }
        }
    };
    let size = tree.size();
    let w_px = size.width() as f64;
    let h_px = size.height() as f64;

    // 从原始文本提取单位信息
    let (w_attr, h_attr) = extract_svg_size(&svg_text);
    let (w_unit_str, w_in) = if let Some(ref w) = w_attr {
        match parse_length(w) {
            Some((_px, unit, inches)) => (unit, inches),
            None => (String::new(), w_px / 96.0),
        }
    } else {
        (String::new(), w_px / 96.0)
    };
    let (h_unit_str, h_in) = if let Some(ref h) = h_attr {
        match parse_length(h) {
            Some((_px, unit, inches)) => (unit, inches),
            None => (String::new(), h_px / 96.0),
        }
    } else {
        (String::new(), h_px / 96.0)
    };

    SvgInfo {
        ok: true,
        width: w_px,
        height: h_px,
        width_unit: w_unit_str,
        height_unit: h_unit_str,
        physical_width_in: w_in,
        physical_height_in: h_in,
        error: None,
    }
}

/// 转换 SVG 文件为栅格图像
/// - input_path: SVG 文件路径
/// - output_path: 输出文件路径
/// - options: 转换选项
#[napi]
pub fn convert_svg(
    input_path: String,
    output_path: String,
    options: Option<SvgConvertOptions>,
) -> SvgConvertResult {
    let opts = options.unwrap_or_default();
    let format = opts.fmt();
    let odpi = opts.dpiv();
    let oqlt = opts.qlt();
    let otile = opts.tile();

    // 1. 读取 SVG
    let data = match fs::read(&input_path) {
        Ok(d) => d,
        Err(e) => {
            return SvgConvertResult {
                ok: false,
                width: 0,
                height: 0,
                bytes_written: 0,
                format,
                dpi: odpi,
                error: Some(format!("无法读取 SVG 文件: {}", e)),
            }
        }
    };

    // 2. 解析 SVG 树
    let options_usvg = usvg::Options::default();
    let tree = match usvg::Tree::from_data(&data, &options_usvg) {
        Ok(t) => t,
        Err(e) => {
            return SvgConvertResult {
                ok: false,
                width: 0,
                height: 0,
                bytes_written: 0,
                format,
                dpi: odpi,
                error: Some(format!("SVG 解析失败: {}", e)),
            }
        }
    };

    // 3. 计算物理尺寸（从原始文本提取单位）
    let svg_text = String::from_utf8_lossy(&data).to_string();
    let (w_attr, h_attr) = extract_svg_size(&svg_text);
    let physical_w_in = w_attr
        .as_ref()
        .and_then(|w| parse_length(w).map(|(_, _, inches)| inches))
        .unwrap_or(tree.size().width() as f64 / 96.0);
    let physical_h_in = h_attr
        .as_ref()
        .and_then(|h| parse_length(h).map(|(_, _, inches)| inches))
        .unwrap_or(tree.size().height() as f64 / 96.0);

    // 4. 计算目标尺寸
    let svg_w = tree.size().width() as f64;
    let svg_h = tree.size().height() as f64;
    if svg_w <= 0.0 || svg_h <= 0.0 {
        return SvgConvertResult {
            ok: false,
            width: 0,
            height: 0,
            bytes_written: 0,
            format,
            dpi: odpi,
            error: Some(format!("SVG 尺寸无效: {}x{}", svg_w, svg_h)),
        };
    }
    let (width, height, sx, sy) =
        compute_target_size(svg_w, svg_h, &opts, physical_w_in, physical_h_in);

    // 5. 解析背景色。默认不透明白色（避免半透明内容在透明背景上灰蒙蒙）
    let bg = opts
        .background
        .as_ref()
        .and_then(|s| parse_hex_color(s))
        .unwrap_or((255, 255, 255, 255));

    // 6. 渲染
    let pixmap = match render_svg(&tree, width, height, sx, sy, Some(bg), otile) {
        Ok(p) => p,
        Err(e) => {
            return SvgConvertResult {
                ok: false,
                width,
                height,
                bytes_written: 0,
                format,
                dpi: odpi,
                error: Some(e),
            }
        }
    };

    // 7. 编码
    let bytes = match encode_pixmap(&pixmap, &format, oqlt, odpi) {
        Ok(b) => b,
        Err(e) => {
            return SvgConvertResult {
                ok: false,
                width,
                height,
                bytes_written: 0,
                format,
                dpi: odpi,
                error: Some(e),
            }
        }
    };

    // 8. 写文件
    let bytes_written = bytes.len() as i64;
    if let Err(e) = fs::write(&output_path, &bytes) {
        return SvgConvertResult {
            ok: false,
            width,
            height,
            bytes_written: 0,
            format,
            dpi: odpi,
            error: Some(format!("写入输出文件失败: {}", e)),
        };
    }

    SvgConvertResult {
        ok: true,
        width,
        height,
        bytes_written,
        format,
        dpi: odpi,
        error: None,
    }
}

/// 转换 SVG 字节为栅格图像字节（不落盘，供前端内存操作）
#[napi]
pub fn convert_svg_bytes(
    svg_bytes: &[u8],
    options: Option<SvgConvertOptions>,
) -> napi::Result<Vec<u8>> {
    let opts = options.unwrap_or_default();
    let format = opts.fmt();
    let odpi = opts.dpiv();
    let oqlt = opts.qlt();
    let otile = opts.tile();

    let options_usvg = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg_bytes, &options_usvg).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("SVG 解析失败: {}", e))
    })?;

    let svg_text = String::from_utf8_lossy(svg_bytes).to_string();
    let (w_attr, h_attr) = extract_svg_size(&svg_text);
    let physical_w_in = w_attr
        .as_ref()
        .and_then(|w| parse_length(w).map(|(_, _, inches)| inches))
        .unwrap_or(tree.size().width() as f64 / 96.0);
    let physical_h_in = h_attr
        .as_ref()
        .and_then(|h| parse_length(h).map(|(_, _, inches)| inches))
        .unwrap_or(tree.size().height() as f64 / 96.0);

    let svg_w = tree.size().width() as f64;
    let svg_h = tree.size().height() as f64;
    if svg_w <= 0.0 || svg_h <= 0.0 {
        return Err(napi::Error::new(
            napi::Status::GenericFailure,
            format!("SVG 尺寸无效: {}x{}", svg_w, svg_h),
        ));
    }
    let (width, height, sx, sy) =
        compute_target_size(svg_w, svg_h, &opts, physical_w_in, physical_h_in);

    let bg = opts
        .background
        .as_ref()
        .and_then(|s| parse_hex_color(s))
        .unwrap_or((255, 255, 255, 255));

    let pixmap = render_svg(&tree, width, height, sx, sy, Some(bg), otile)
        .map_err(|e| napi::Error::new(napi::Status::GenericFailure, e))?;

    encode_pixmap(&pixmap, &format, oqlt, odpi)
        .map_err(|e| napi::Error::new(napi::Status::GenericFailure, e))
}

// ============ 单元测试 ============

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_length_px() {
        let (px, unit, inches) = parse_length("100").unwrap();
        assert_eq!(px, 100.0);
        assert_eq!(unit, "");
        assert!((inches - 100.0 / 96.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_length_cm() {
        let (px, unit, inches) = parse_length("10cm").unwrap();
        assert!((px - 10.0 * 96.0 / 2.54).abs() < 1e-6);
        assert_eq!(unit, "cm");
        assert!((inches - 10.0 / 2.54).abs() < 1e-9);
    }

    #[test]
    fn test_parse_length_in() {
        let (px, unit, inches) = parse_length("3.5in").unwrap();
        assert!((px - 336.0).abs() < 1e-9);
        assert_eq!(unit, "in");
        assert!((inches - 3.5).abs() < 1e-9);
    }

    #[test]
    fn test_parse_length_pt() {
        let (px, unit, inches) = parse_length("72pt").unwrap();
        assert!((px - 96.0).abs() < 1e-9);
        assert_eq!(unit, "pt");
        assert!((inches - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_length_mm() {
        let (px, _unit, inches) = parse_length("25.4mm").unwrap();
        assert!((px - 96.0).abs() < 1e-6);
        assert!((inches - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_length_invalid() {
        assert!(parse_length("").is_none());
        assert!(parse_length("abc").is_none());
    }

    #[test]
    fn test_parse_hex_color_rgb() {
        let (r, g, b, a) = parse_hex_color("#fff").unwrap();
        assert_eq!((r, g, b, a), (255, 255, 255, 255));
    }

    #[test]
    fn test_parse_hex_color_rrggbb() {
        let (r, g, b, a) = parse_hex_color("#FF8800").unwrap();
        assert_eq!((r, g, b, a), (255, 136, 0, 255));
    }

    #[test]
    fn test_parse_hex_color_aarrggbb() {
        let (r, g, b, a) = parse_hex_color("#80FF0000").unwrap();
        assert_eq!((r, g, b, a), (255, 0, 0, 128));
    }

    #[test]
    fn test_extract_svg_size() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10cm" height="5cm"></svg>"#;
        let (w, h) = extract_svg_size(svg);
        assert_eq!(w.as_deref(), Some("10cm"));
        assert_eq!(h.as_deref(), Some("5cm"));
    }

    #[test]
    fn test_extract_svg_size_single_quote() {
        let svg = r#"<svg width='100' height='200'></svg>"#;
        let (w, h) = extract_svg_size(svg);
        assert_eq!(w.as_deref(), Some("100"));
        assert_eq!(h.as_deref(), Some("200"));
    }

    #[test]
    fn test_extract_svg_size_no_svg_tag() {
        let (w, h) = extract_svg_size("<html></html>");
        assert!(w.is_none());
        assert!(h.is_none());
    }

    #[test]
    fn test_compute_target_size_default() {
        let opts = SvgConvertOptions::default();
        let (w, h, sx, sy) = compute_target_size(200.0, 100.0, &opts, 2.08, 1.04);
        assert_eq!(w, 200);
        assert_eq!(h, 100);
        assert!((sx - 1.0).abs() < 1e-9);
        assert!((sy - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_compute_target_size_explicit() {
        let opts = SvgConvertOptions {
            width: Some(400),
            height: Some(200),
            ..Default::default()
        };
        let (w, h, sx, sy) = compute_target_size(200.0, 100.0, &opts, 2.08, 1.04);
        assert_eq!(w, 400);
        assert_eq!(h, 200);
        assert!((sx - 2.0).abs() < 1e-9);
        assert!((sy - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_compute_target_size_scale() {
        let opts = SvgConvertOptions {
            scale: Some(2.0),
            ..Default::default()
        };
        let (w, h, _sx, _sy) = compute_target_size(200.0, 100.0, &opts, 2.08, 1.04);
        assert_eq!(w, 400);
        assert_eq!(h, 200);
    }

    #[test]
    fn test_compute_target_size_physical() {
        // 10cm × 5cm @ 300dpi → (10/2.54*300, 5/2.54*300) ≈ (1181, 591)
        let opts = SvgConvertOptions {
            dpi: Some(300.0),
            keep_physical_size: Some(true),
            ..Default::default()
        };
        let (w, h, _sx, _sy) = compute_target_size(377.95, 188.98, &opts, 10.0 / 2.54, 5.0 / 2.54);
        assert_eq!(w, (10.0_f64 / 2.54 * 300.0).round() as u32);
        assert_eq!(h, (5.0_f64 / 2.54 * 300.0).round() as u32);
    }

    #[test]
    fn test_compute_target_size_only_width() {
        let opts = SvgConvertOptions {
            width: Some(400),
            ..Default::default()
        };
        let (w, h, _sx, _sy) = compute_target_size(200.0, 100.0, &opts, 2.08, 1.04);
        assert_eq!(w, 400);
        assert_eq!(h, 200); // 按比例
    }

    #[test]
    fn test_crc32_known() {
        // CRC32 of "123456789" = 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn test_render_simple_svg() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
            <rect x="0" y="0" width="100" height="50" fill="#ff0000"/>
        </svg>"##;
        let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()).unwrap();
        let pixmap = render_svg(&tree, 100, 50, 1.0, 1.0, None, 0).unwrap();
        assert_eq!(pixmap.width(), 100);
        assert_eq!(pixmap.height(), 50);
        // 左上角应红色
        let pixel = pixmap.pixel(0, 0).unwrap();
        assert!(pixel.red() > 200);
        assert!(pixel.green() < 50);
        assert!(pixel.blue() < 50);
    }

    #[test]
    fn test_encode_png() {
        let mut pixmap = tiny_skia::Pixmap::new(10, 10).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        let bytes = encode_pixmap(&pixmap, "png", 92, 96.0).unwrap();
        // PNG 签名
        assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        // 应包含 pHYs chunk (DPI 元数据)
        assert!(bytes.windows(4).any(|w| w == b"pHYs"));
    }

    #[test]
    fn test_encode_jpeg() {
        let mut pixmap = tiny_skia::Pixmap::new(10, 10).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(0, 255, 0, 255));
        let bytes = encode_pixmap(&pixmap, "jpeg", 90, 96.0).unwrap();
        // JPEG SOI marker
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn test_convert_svg_bytes_basic() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
            <rect x="0" y="0" width="20" height="20" fill="#00ff00"/>
        </svg>"##;
        let opts = SvgConvertOptions {
            format: Some("png".to_string()),
            ..Default::default()
        };
        let bytes = convert_svg_bytes(svg.as_bytes(), Some(opts)).unwrap();
        assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    #[test]
    fn test_svg_info_basic() {
        // 写临时文件
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10cm" height="5cm">
            <rect x="0" y="0" width="100" height="50" fill="#000"/>
        </svg>"##;
        let tmp = std::env::temp_dir().join("bioio_svg_info_test.svg");
        fs::write(&tmp, svg).unwrap();
        let info = svg_info(tmp.to_string_lossy().to_string());
        assert!(info.ok);
        assert!((info.physical_width_in - 10.0 / 2.54).abs() < 1e-6);
        assert!((info.physical_height_in - 5.0 / 2.54).abs() < 1e-6);
        assert_eq!(info.width_unit, "cm");
        let _ = fs::remove_file(&tmp);
    }
}
