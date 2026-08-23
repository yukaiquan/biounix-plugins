// BioUnix SVG → 光栅图像转换模块
// 独立 crate，按需安装（非内置，体积约 20MB）
// 通过 napi-rs 暴露给 Node.js / Electron 主进程
//
// 本模块从 bioio 拆分而来，专责 SVG 渲染：
//   - resvg + usvg + tiny-skia：矢量渲染引擎
//   - ab_glyph：字体渲染（SVG 文本 outline）
//   - quick-xml：流式解析超大 SVG（避免 OOM）
//   - image：多格式编码（PNG/JPEG/TIFF/BMP）
//
// 因依赖体积大（~20MB），不内置打包，用户从工具商店按需下载。
#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

// SVG → 栅格图像转换引擎（resvg + tiny-skia + image）
pub mod svg_convert;
// 混合流式 SVG 渲染器（针对超大 SVG，quick-xml + tiny-skia + ab_glyph）
pub mod svg_fast_render;
