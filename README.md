# BioUnix Plugins

BioUnix 可拆卸 Rust 原生模块仓库。

通过 napi-rs 编译为 `.node` 文件，供 BioUnix Electron 主进程按需加载。每个模块独立编译、独立版本、独立发布到 GitHub Releases，BioUnix 工具商店从本仓库拉取 `modules-index.json` 列出可安装模块。

## 模块清单

| 模块           | 版本  | 说明                                                   | 内置？      |
| -------------- | ----- | ------------------------------------------------------ | ----------- |
| `biounix-core` | 0.1.0 | 核心纯计算（序列分析 + TF-IDF/Embedding + Token 估算） | ✅ 默认安装 |
| `biounix-io`   | 0.1.0 | 文件 I/O（FASTA/FASTQ/BAM/VCF/BCF/GFF/BED 解析统计）   | ✅ 默认安装 |
| `biounix-svg`  | 0.1.0 | SVG → 光栅图像转换引擎（resvg + tiny-skia）            | ❌ 按需下载 |

> 内置模块随 BioUnix app 打包（asar.unpacked），按需模块从 GitHub Releases 下载到 `<appData>/rust-modules/`。

## 目录结构

```
biounix-plugins/
├── crates/
│   ├── biounix-core/        # 核心纯计算
│   │   ├── Cargo.toml
│   │   ├── build.rs
│   │   └── src/lib.rs
│   ├── biounix-io/          # 文件 I/O
│   │   ├── Cargo.toml
│   │   ├── build.rs
│   │   └── src/lib.rs
│   └── biounix-svg/         # SVG 转换
│       ├── Cargo.toml
│       ├── build.rs
│       └── src/
│           ├── lib.rs
│           ├── svg_convert.rs
│           └── svg_fast_render.rs
├── scripts/
│   └── build.js             # 本地构建脚本（编译当前平台 + 复制 .node）
├── modules-index.json       # 模块索引（供 BioUnix 工具商店拉取）
├── .github/workflows/
│   ├── build.yml            # 跨平台编译 + 自动 release
│   └── update-index.yml     # 更新 modules-index.json
└── README.md
```

## 本地构建

```bash
# 安装 Rust 工具链
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 编译当前平台所有模块
node scripts/build.js

# 编译指定模块
node scripts/build.js --module biounix-core

# 产物：crates/<module>/<module>.<platform>-<arch>.node
# 例如：crates/biounix-core/biounix-core.darwin-arm64.node
```

## CI 自动发布

推送到 `main` 分支或手动触发 workflow 时，GitHub Actions 会：

1. 在 4 个平台（macOS arm64/x64、Linux x64、Windows x64）并行编译所有模块
2. 为每个模块生成 `manifest-<version>.json`（含各平台 .node 的下载 URL + SHA-256）
3. 创建 GitHub Release（tag: `<module>-v<version>`），上传 `.node` + manifest 作为 asset
4. 更新 `modules-index.json` 并提交回仓库

BioUnix app 的工具商店从 `modules-index.json` 拉取可安装列表，用户点击安装后从对应 release 下载 `.node` + 校验 SHA-256。

## 下载协议

BioUnix 安装器（`src/main/services/system/rust-modules/installer.ts`）的下载流程：

1. 拉取 `modules-index.json` → 找到模块的 `release_url`
2. 从 `release_url` 推导 manifest URL：`/releases/tag/` → `/releases/download/` + `manifest-<version>.json`
3. 校验 manifest 签名（ed25519，开发模式跳过）
4. 从 `manifest.platforms["<platform>-<arch>"]` 取 `.node` 下载 URL + sha256
5. 下载 → 校验 SHA-256 → 落盘到 `<appData>/rust-modules/<name>/<version>/`

## License

MIT
