#!/usr/bin/env node
/**
 * 本地构建脚本：编译当前平台的所有 Rust napi-rs 模块并复制 .node 到 crates/<module>/
 *
 * 产物命名：<module>.<platform>-<arch>.node（与 BioUnix registry.getNativeFileName 一致）
 *
 * 用法:
 *   node scripts/build.js                      # 编译当前平台所有模块
 *   node scripts/build.js --module biounix-core # 仅编译指定模块
 *
 * 交叉编译请用 GitHub Actions（.github/workflows/build.yml），本地仅支持当前平台。
 */
const { execSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const REPO_ROOT = path.join(__dirname, '..');
const CRATES_DIR = path.join(REPO_ROOT, 'crates');
const ALL_MODULES = ['biounix-core', 'biounix-io', 'biounix-svg'];

// target triple → (platformName, arch, libExt, libPrefix)
// platformName/arch 必须与 process.platform/process.arch 一致，用于 .node 命名
const TARGETS = {
    'aarch64-apple-darwin': { platform: 'darwin', arch: 'arm64', ext: 'dylib', prefix: 'lib' },
    'x86_64-apple-darwin': { platform: 'darwin', arch: 'x64', ext: 'dylib', prefix: 'lib' },
    'x86_64-unknown-linux-gnu': { platform: 'linux', arch: 'x64', ext: 'so', prefix: 'lib' },
    'x86_64-pc-windows-msvc': { platform: 'win32', arch: 'x64', ext: 'dll', prefix: '' },
};

function currentTriple () {
    const { platform, arch } = process;
    if (platform === 'darwin') return arch === 'arm64' ? 'aarch64-apple-darwin' : 'x86_64-apple-darwin';
    if (platform === 'linux') return 'x86_64-unknown-linux-gnu';
    if (platform === 'win32') return 'x86_64-pc-windows-msvc';
    throw new Error(`不支持的平台: ${platform}-${arch}`);
}

function buildOne (triple, moduleName) {
    const cfg = TARGETS[triple];
    if (!cfg) throw new Error(`未知 target: ${triple}`);

    const crateDir = path.join(CRATES_DIR, moduleName);
    if (!fs.existsSync(path.join(crateDir, 'Cargo.toml'))) {
        throw new Error(`crate 不存在: ${crateDir}/Cargo.toml`);
    }

    const nodeFileName = `${moduleName}.${cfg.platform}-${cfg.arch}.node`;
    // Rust lib 名：连字符替换为下划线（Cargo 规范）
    const libName = `${cfg.prefix}${moduleName.replace(/-/g, '_')}.${cfg.ext}`;
    console.log(`[build] ==> ${moduleName} (${triple}) → ${nodeFileName}`);

    execSync(`cargo build --release --target ${triple}`, { cwd: crateDir, stdio: 'inherit' });

    const libFile = path.join(crateDir, 'target', triple, 'release', libName);
    if (!fs.existsSync(libFile)) {
        throw new Error(`构建产物未找到: ${libFile}`);
    }
    const dest = path.join(crateDir, nodeFileName);
    fs.copyFileSync(libFile, dest);
    console.log(`[build]     已复制到: ${path.relative(REPO_ROOT, dest)}`);
}

function main () {
    const args = process.argv.slice(2);
    const modIdx = args.indexOf('--module');
    const modulesToBuild = modIdx !== -1 && args[modIdx + 1]
        ? [args[modIdx + 1]]
        : ALL_MODULES;

    const triple = currentTriple();
    console.log(`[build] 当前平台: ${process.platform}-${process.arch} → ${triple}`);
    console.log(`[build] 待编译模块: ${modulesToBuild.join(', ')}`);

    for (const mod of modulesToBuild) {
        buildOne(triple, mod);
    }
    console.log('[build] 全部完成。');
}

main();
