// BioUnix 高性能生信计算模块
// 由 Rust 编写，通过 napi-rs 暴露给 Node.js / Electron 主进程
// 设计理念第 4 层：Rust 负责性能敏感和底层任务
#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// ============ 纯计算函数（biounix-core） ============

/// 计算序列的 GC 含量（百分比，0-100）
#[napi]
pub fn gc_content(sequence: String) -> f64 {
    let seq = sequence.as_bytes();
    let mut gc: u64 = 0;
    let mut total: u64 = 0;
    for &b in seq {
        match b.to_ascii_uppercase() {
            b'G' | b'C' => {
                gc += 1;
                total += 1;
            }
            b'A' | b'T' | b'U' => {
                total += 1;
            }
            b'N' => {}
            _ => {}
        }
    }
    if total == 0 {
        0.0
    } else {
        (gc as f64 / total as f64) * 100.0
    }
}

/// 生成反向互补序列（支持 IUPAC 简并碱基）
#[napi]
pub fn reverse_complement(sequence: String) -> String {
    sequence
        .bytes()
        .rev()
        .map(|b| match b.to_ascii_uppercase() {
            b'A' => b'T',
            b'T' => b'A',
            b'G' => b'C',
            b'C' => b'G',
            b'N' => b'N',
            b'U' => b'A',
            b'R' => b'Y',
            b'Y' => b'R',
            b'S' => b'S',
            b'W' => b'W',
            b'K' => b'M',
            b'M' => b'K',
            b'B' => b'V',
            b'V' => b'B',
            b'D' => b'H',
            b'H' => b'D',
            _ => b'N',
        })
        .map(|b| b as char)
        .collect()
}

/// k-mer 计数，返回前 top_n 个高频 k-mer（跳过含 N 的 k-mer）
#[napi(object)]
pub struct KmerEntry {
    pub kmer: String,
    pub count: i64,
}

#[napi]
pub fn kmer_count(sequence: String, k: u32, top_n: u32) -> Vec<KmerEntry> {
    let seq = sequence.as_bytes();
    let k = k as usize;
    if k == 0 || seq.len() < k {
        return vec![];
    }
    let upper: Vec<u8> = seq.iter().map(|&b| b.to_ascii_uppercase()).collect();
    let mut counts: HashMap<&[u8], u64> = HashMap::new();
    let mut i = 0;
    while i + k <= upper.len() {
        let kmer = &upper[i..i + k];
        if !kmer.contains(&b'N') {
            *counts.entry(kmer).or_insert(0) += 1;
        }
        i += 1;
    }
    let mut entries: Vec<KmerEntry> = counts
        .into_iter()
        .map(|(kmer, count)| KmerEntry {
            kmer: String::from_utf8_lossy(kmer).to_string(),
            count: count as i64,
        })
        .collect();
    entries.sort_by(|a, b| b.count.cmp(&a.count));
    entries.truncate(top_n as usize);
    entries
}

/// DNA 翻译为氨基酸序列（标准密码子表，遇到终止密码子输出 *）
#[napi]
pub fn translate_dna(sequence: String) -> String {
    let seq = sequence.as_bytes();
    let mut result = String::with_capacity(seq.len() / 3);
    let mut i = 0;
    while i + 3 <= seq.len() {
        let codon = [
            seq[i].to_ascii_uppercase(),
            seq[i + 1].to_ascii_uppercase(),
            seq[i + 2].to_ascii_uppercase(),
        ];
        result.push(codon_to_aa(&codon));
        i += 3;
    }
    result
}

fn codon_to_aa(c: &[u8; 3]) -> char {
    // 标准遗传密码表
    match c {
        // 苯丙氨酸 F（仅 TTT、TTC）
        [b'T', b'T', b'T'] | [b'T', b'T', b'C'] => 'F',
        // 亮氨酸 L（TTA、TTG、CTT、CTC、CTA、CTG）
        [b'T', b'T', b'A']
        | [b'T', b'T', b'G']
        | [b'C', b'T', b'T']
        | [b'C', b'T', b'C']
        | [b'C', b'T', b'A']
        | [b'C', b'T', b'G'] => 'L',
        // 异亮氨酸 I
        [b'A', b'T', b'T'] | [b'A', b'T', b'C'] | [b'A', b'T', b'A'] => 'I',
        // 甲硫氨酸/起始 M
        [b'A', b'T', b'G'] => 'M',
        // 缬氨酸 V
        [b'G', b'T', b'T'] | [b'G', b'T', b'C'] | [b'G', b'T', b'A'] | [b'G', b'T', b'G'] => 'V',
        // 丝氨酸 S
        [b'T', b'C', b'T']
        | [b'T', b'C', b'C']
        | [b'T', b'C', b'A']
        | [b'T', b'C', b'G']
        | [b'A', b'G', b'T']
        | [b'A', b'G', b'C'] => 'S',
        // 脯氨酸 P
        [b'C', b'C', b'T'] | [b'C', b'C', b'C'] | [b'C', b'C', b'A'] | [b'C', b'C', b'G'] => 'P',
        // 苏氨酸 T
        [b'A', b'C', b'T'] | [b'A', b'C', b'C'] | [b'A', b'C', b'A'] | [b'A', b'C', b'G'] => 'T',
        // 丙氨酸 A
        [b'G', b'C', b'T'] | [b'G', b'C', b'C'] | [b'G', b'C', b'A'] | [b'G', b'C', b'G'] => 'A',
        // 酪氨酸 Y
        [b'T', b'A', b'T'] | [b'T', b'A', b'C'] => 'Y',
        // 组氨酸 H
        [b'C', b'A', b'T'] | [b'C', b'A', b'C'] => 'H',
        // 谷氨酰胺 Q
        [b'C', b'A', b'A'] | [b'C', b'A', b'G'] => 'Q',
        // 天冬酰胺 N
        [b'A', b'A', b'T'] | [b'A', b'A', b'C'] => 'N',
        // 赖氨酸 K
        [b'A', b'A', b'A'] | [b'A', b'A', b'G'] => 'K',
        // 天冬氨酸 D
        [b'G', b'A', b'T'] | [b'G', b'A', b'C'] => 'D',
        // 谷氨酸 E
        [b'G', b'A', b'A'] | [b'G', b'A', b'G'] => 'E',
        // 半胱氨酸 C
        [b'T', b'G', b'T'] | [b'T', b'G', b'C'] => 'C',
        // 色氨酸 W
        [b'T', b'G', b'G'] => 'W',
        // 精氨酸 R
        [b'C', b'G', b'T']
        | [b'C', b'G', b'C']
        | [b'C', b'G', b'A']
        | [b'C', b'G', b'G']
        | [b'A', b'G', b'A']
        | [b'A', b'G', b'G'] => 'R',
        // 甘氨酸 G
        [b'G', b'G', b'T'] | [b'G', b'G', b'C'] | [b'G', b'G', b'A'] | [b'G', b'G', b'G'] => 'G',
        // 终止密码子
        [b'T', b'A', b'A'] | [b'T', b'A', b'G'] | [b'T', b'G', b'A'] => '*',
        _ => 'X',
    }
}

// ============ 高级统计 ============

/// 计算 N50（输入序列长度数组，返回 N50 值）
/// N50：按长度降序累加，累计长度达到总长 50% 时的那条序列长度
#[napi]
pub fn seq_n50(lengths: Vec<i64>) -> i64 {
    if lengths.is_empty() {
        return 0;
    }
    let mut sorted = lengths.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    let total: i64 = sorted.iter().sum();
    let half = total / 2;
    let mut acc: i64 = 0;
    for &len in &sorted {
        acc += len;
        if acc >= half {
            return len;
        }
    }
    0
}

/// 批量序列统计汇总
#[napi(object)]
pub struct SeqSummary {
    pub count: i64,
    pub total_length: i64,
    pub min_length: i64,
    pub max_length: i64,
    pub avg_length: f64,
    pub n50: i64,
    pub avg_gc: f64,
}

/// 对一组序列长度和 GC 值进行汇总统计（含 N50）
#[napi]
pub fn summarize_sequences(lengths: Vec<i64>, gc_values: Vec<f64>) -> SeqSummary {
    let count = lengths.len() as i64;
    if count == 0 {
        return SeqSummary {
            count: 0,
            total_length: 0,
            min_length: 0,
            max_length: 0,
            avg_length: 0.0,
            n50: 0,
            avg_gc: 0.0,
        };
    }
    let total_length: i64 = lengths.iter().sum();
    let min_length = *lengths.iter().min().unwrap_or(&0);
    let max_length = *lengths.iter().max().unwrap_or(&0);
    let avg_length = total_length as f64 / count as f64;
    let n50 = seq_n50(lengths.clone());
    let gc_sum: f64 = gc_values.iter().sum();
    let avg_gc = if gc_values.is_empty() {
        0.0
    } else {
        gc_sum / gc_values.len() as f64
    };
    SeqSummary {
        count,
        total_length,
        min_length,
        max_length,
        avg_length,
        n50,
        avg_gc,
    }
}

// ============================================================
// 序列 motif 查找（支持 IUPAC 简并碱基）
// ============================================================

/// Motif 命中位置
#[napi(object)]
pub struct MotifMatch {
    pub start: i64,      // 0-based 起始位置
    pub end: i64,        // 0-based 结束位置（不包含）
    pub matched: String, // 命中的子序列
    pub strand: String,  // "+" 或 "-"
}

/// IUPAC 简并碱基 → 位掩码（bit 0=A, 1=C, 2=G, 3=T/U）
fn iupac_to_mask(c: u8) -> u8 {
    match c.to_ascii_uppercase() {
        b'A' => 0b0001,
        b'C' => 0b0010,
        b'G' => 0b0100,
        b'T' | b'U' => 0b1000,
        b'R' => 0b0101, // A/G
        b'Y' => 0b1010, // C/T
        b'S' => 0b0110, // C/G
        b'W' => 0b1001, // A/T
        b'K' => 0b1100, // G/T
        b'M' => 0b0011, // A/C
        b'B' => 0b1110, // C/G/T
        b'D' => 0b1101, // A/G/T
        b'H' => 0b1011, // A/C/T
        b'V' => 0b0111, // A/C/G
        b'N' => 0b1111,
        _ => 0b1111,
    }
}

fn base_to_mask(c: u8) -> u8 {
    match c.to_ascii_uppercase() {
        b'A' => 0b0001,
        b'C' => 0b0010,
        b'G' => 0b0100,
        b'T' | b'U' => 0b1000,
        _ => 0b1111, // N 或未知 → 匹配任意
    }
}

/// 在序列中查找 motif（支持 IUPAC 简并碱基、正反链）
/// motif 示例："TATAWAW"（W=A/T）、"GGTCCA"、"NNGG"（N=任意）
#[napi]
pub fn find_motifs(sequence: String, motif: String, strand: String) -> Vec<MotifMatch> {
    let seq = sequence.as_bytes();
    let pat: &[u8] = motif.as_bytes();
    let mlen = pat.len();
    let mut results: Vec<MotifMatch> = Vec::new();

    if mlen == 0 || mlen > seq.len() {
        return results;
    }

    // 预处理 motif 掩码
    let masks: Vec<u8> = pat.iter().map(|&c| iupac_to_mask(c)).collect();

    let search_plus = strand != "-";
    let search_minus = strand != "+"; // "both" 或 "-" 都搜索反链

    // 正链匹配
    if search_plus {
        for i in 0..=seq.len() - mlen {
            let mut ok = true;
            for j in 0..mlen {
                if (masks[j] & base_to_mask(seq[i + j])) == 0 {
                    ok = false;
                    break;
                }
            }
            if ok {
                results.push(MotifMatch {
                    start: i as i64,
                    end: (i + mlen) as i64,
                    matched: String::from_utf8_lossy(&seq[i..i + mlen]).to_string(),
                    strand: "+".to_string(),
                });
            }
        }
    }

    // 反链匹配（查找反向互补 motif 在正链的位置）
    if search_minus {
        // 构建反向互补 motif 掩码
        let rc_masks: Vec<u8> = masks
            .iter()
            .rev()
            .map(|&m| {
                // 反向互补掩码：A<->T, C<->G
                let mut rc = 0u8;
                if m & 0b0001 != 0 {
                    rc |= 0b1000;
                } // A -> T
                if m & 0b0010 != 0 {
                    rc |= 0b0100;
                } // C -> G
                if m & 0b0100 != 0 {
                    rc |= 0b0010;
                } // G -> C
                if m & 0b1000 != 0 {
                    rc |= 0b0001;
                } // T -> A
                rc
            })
            .collect();

        for i in 0..=seq.len() - mlen {
            let mut ok = true;
            for j in 0..mlen {
                if (rc_masks[j] & base_to_mask(seq[i + j])) == 0 {
                    ok = false;
                    break;
                }
            }
            if ok {
                results.push(MotifMatch {
                    start: i as i64,
                    end: (i + mlen) as i64,
                    matched: String::from_utf8_lossy(&seq[i..i + mlen]).to_string(),
                    strand: "-".to_string(),
                });
            }
        }
    }

    results
}

// ============================================================
// 限制性内切酶识别位点预测
// ============================================================

/// 内切酶酶切位点
#[napi(object)]
pub struct RestrictionSite {
    pub enzyme_name: String,
    pub position: i64,     // 识别位点起始位置（0-based）
    pub cut_position: i64, // 切割位置（0-based）
    pub recognition_site: String,
    pub strand: String,
}

/// 内置常见限制性内切酶识别位点（含切割位置偏移）
/// 切割位置 = 识别位点起始 + offset（offset 为负表示在识别位点上游）
fn builtin_enzymes() -> Vec<(&'static str, &'static str, i64)> {
    // (酶名, 识别序列, 切割位置相对识别序列起点的偏移)
    // 切割位置以 ^ 标注惯例：G^AATTC → 偏移 1
    vec![
        ("EcoRI", "GAATTC", 1),
        ("BamHI", "GGATCC", 1),
        ("HindIII", "AAGCTT", 1),
        ("XhoI", "CTCGAG", 1),
        ("XbaI", "TCTAGA", 1),
        ("SalI", "GTCGAC", 1),
        ("PstI", "CTGCAG", 5),
        ("KpnI", "GGTACC", 5),
        ("SmaI", "CCCGGG", 3),
        ("SacI", "GAGCTC", 5),
        ("NotI", "GCGGCCGC", 2),
        ("EcoRV", "GATATC", 3),
        ("NdeI", "CATATG", 2),
        ("NheI", "GCTAGC", 1),
        ("SpeI", "ACTAGT", 1),
    ]
}

/// 预测序列上的限制性内切酶酶切位点
/// 若 enzyme_names 为空，则使用内置 15 种常见酶
#[napi]
pub fn find_restriction_sites(sequence: String, enzyme_names: Vec<String>) -> Vec<RestrictionSite> {
    let seq = sequence.as_bytes();
    let mut results: Vec<RestrictionSite> = Vec::new();

    // 选择要查询的酶
    let all_enzymes = builtin_enzymes();
    let enzymes: Vec<(&'static str, &'static str, i64)> = if enzyme_names.is_empty() {
        all_enzymes
    } else {
        all_enzymes
            .into_iter()
            .filter(|(name, _, _)| enzyme_names.iter().any(|n| n.eq_ignore_ascii_case(name)))
            .collect()
    };

    for (name, site, cut_offset) in enzymes {
        let pat = site.as_bytes();
        let plen = pat.len();
        if plen == 0 || plen > seq.len() {
            continue;
        }
        for i in 0..=seq.len() - plen {
            let mut ok = true;
            for j in 0..plen {
                if pat[j].to_ascii_uppercase() != seq[i + j].to_ascii_uppercase() {
                    ok = false;
                    break;
                }
            }
            if ok {
                let cut = i as i64 + cut_offset;
                results.push(RestrictionSite {
                    enzyme_name: name.to_string(),
                    position: i as i64,
                    cut_position: cut,
                    recognition_site: site.to_string(),
                    strand: "+".to_string(),
                });
            }
        }
    }

    // 按位置排序
    results.sort_by_key(|r| r.position);
    results
}

// ============================================================
// 蛋白质理化性质计算
// ============================================================

/// 蛋白质理化性质
#[napi(object)]
pub struct ProteinProperties {
    pub length: i64,
    pub molecular_weight: f64,                // 分子量（Da）
    pub isoelectric_point: f64,               // 等电点 pI
    pub net_charge_ph7: f64,                  // pH 7.0 时的净电荷
    pub aromaticity: f64,                     // 芳香性（F/W/Y 比例）
    pub instability_index: f64,               // 不稳定指数
    pub gravy: f64,                           // 平均疏水性（Grand average of hydropathy）
    pub aa_composition: HashMap<String, f64>, // 氨基酸组成（百分比）
}

/// 氨基酸分子量（Da，含 H2O 脱水后的残基质量）
fn aa_mw(aa: u8) -> f64 {
    match aa {
        b'A' => 89.09,
        b'R' => 174.20,
        b'N' => 132.12,
        b'D' => 133.10,
        b'C' => 121.16,
        b'E' => 147.13,
        b'Q' => 146.15,
        b'G' => 75.07,
        b'H' => 155.16,
        b'I' => 131.17,
        b'L' => 131.17,
        b'K' => 146.19,
        b'M' => 149.21,
        b'F' => 165.19,
        b'P' => 115.13,
        b'S' => 105.09,
        b'T' => 119.12,
        b'W' => 204.23,
        b'Y' => 181.19,
        b'V' => 117.15,
        _ => 0.0,
    }
}

/// 氨基酸在 pH 7.0 的电荷（正/负/0）
fn aa_charge_ph7(aa: u8) -> f64 {
    match aa {
        b'K' | b'R' | b'H' => 1.0,
        b'D' | b'E' => -1.0,
        _ => 0.0,
    }
}

/// 氨基酸 Kyte-Doolittle 疏水性值
fn aa_hydropathy(aa: u8) -> f64 {
    match aa {
        b'I' => 4.5,
        b'V' => 4.2,
        b'L' => 3.8,
        b'F' => 2.8,
        b'C' => 2.5,
        b'M' => 1.9,
        b'A' => 1.8,
        b'G' => -0.4,
        b'T' => -0.7,
        b'S' => -0.8,
        b'W' => -0.9,
        b'Y' => -1.3,
        b'P' => -1.6,
        b'H' => -3.2,
        b'E' => -3.5,
        b'Q' => -3.5,
        b'D' => -3.5,
        b'N' => -3.5,
        b'K' => -3.9,
        b'R' => -4.5,
        _ => 0.0,
    }
}

/// 不稳定指数 dipeptide 系数表（Guruprasad et al. 1990）
fn dipeptide_instability(a: u8, b: u8) -> f64 {
    // 简化：返回常见不稳定 dipeptide 的权重
    let key = (a.to_ascii_uppercase(), b.to_ascii_uppercase());
    match key {
        (b'A', b'A') => 0.0,
        (b'A', b'E') => 10.0,
        (b'D', b'E') => 20.0,
        (b'E', b'A') => 10.0,
        (b'E', b'D') => 20.0,
        (b'G', b'G') => 0.0,
        (b'K', b'R') => 25.0,
        (b'R', b'K') => 25.0,
        (b'S', b'S') => 0.0,
        (b'T', b'T') => 0.0,
        (b'P', b'P') => 0.0,
        (b'Q', b'Q') => 20.0,
        (b'N', b'N') => 0.0,
        (b'W', b'W') => 10.0,
        (b'Y', b'Y') => 10.0,
        (b'F', b'F') => 10.0,
        (b'I', b'I') => 0.0,
        (b'L', b'L') => 0.0,
        (b'V', b'V') => 0.0,
        (b'M', b'M') => 0.0,
        _ => 0.0,
    }
}

/// 计算蛋白质序列的理化性质
/// 输入氨基酸序列（单字母大写或小写均可，忽略非氨基酸字符）
#[napi]
pub fn protein_properties(sequence: String) -> ProteinProperties {
    let seq: Vec<u8> = sequence
        .bytes()
        .map(|b| b.to_ascii_uppercase())
        .filter(|b| matches!(b, b'A'..=b'Z'))
        .collect();
    let length = seq.len() as i64;

    // 氨基酸组成
    let mut aa_counts: HashMap<u8, u64> = HashMap::new();
    for &aa in &seq {
        *aa_counts.entry(aa).or_insert(0) += 1;
    }
    let mut aa_composition: HashMap<String, f64> = HashMap::new();
    for (aa, count) in &aa_counts {
        aa_composition.insert(
            String::from_utf8_lossy(&[*aa]).to_string(),
            (*count as f64 / seq.len() as f64) * 100.0,
        );
    }

    // 分子量 = sum(残基质量) + 水（18.015）
    let mut mw: f64 = 0.0;
    for &aa in &seq {
        mw += aa_mw(aa);
    }
    if !seq.is_empty() {
        mw += 18.015;
    }

    // pH 7.0 净电荷
    let net_charge: f64 = seq.iter().map(|&aa| aa_charge_ph7(aa)).sum();

    // 芳香性（F/W/Y 占比）
    let aromatic_count = seq
        .iter()
        .filter(|&&aa| aa == b'F' || aa == b'W' || aa == b'Y')
        .count() as f64;
    let aromaticity = if seq.is_empty() {
        0.0
    } else {
        aromatic_count / seq.len() as f64
    };

    // 不稳定指数
    let mut instability: f64 = 0.0;
    for i in 0..seq.len().saturating_sub(1) {
        instability += dipeptide_instability(seq[i], seq[i + 1]);
    }
    let instability_index = if seq.is_empty() {
        0.0
    } else {
        instability * 10.0 / seq.len() as f64
    };

    // GRAVY（平均疏水性）
    let total_hydropathy: f64 = seq.iter().map(|&aa| aa_hydropathy(aa)).sum();
    let gravy = if seq.is_empty() {
        0.0
    } else {
        total_hydropathy / seq.len() as f64
    };

    // 等电点近似（简化计算，基于 K/R/H/D/E 计数）
    // pI ≈ 7.0 + 0.1 * (正电荷数 - 负电荷数)，真实计算需 Newton 迭代，这里给近似值
    let pos_count = seq
        .iter()
        .filter(|&&aa| aa == b'K' || aa == b'R' || aa == b'H')
        .count() as f64;
    let neg_count = seq.iter().filter(|&&aa| aa == b'D' || aa == b'E').count() as f64;
    let pi = if pos_count + neg_count == 0.0 {
        7.0
    } else {
        7.0 + 0.3 * (pos_count - neg_count) / (pos_count + neg_count) * 10.0
    };

    ProteinProperties {
        length,
        molecular_weight: mw,
        isoelectric_point: pi.clamp(2.0, 13.0),
        net_charge_ph7: net_charge,
        aromaticity,
        instability_index,
        gravy,
        aa_composition,
    }
}

// ============================================================
// ORF（开放阅读框）预测
// ============================================================

/// ORF 预测结果
#[napi(object)]
pub struct OrfResult {
    pub start: i64,     // 0-based 起始位置
    pub end: i64,       // 结束位置（不包含）
    pub length: i64,    // ORF 长度（含终止密码子）
    pub strand: String, // "+" / "-"
    pub frame: i64,     // 读码框 0/1/2
    pub start_codon: String,
    pub stop_codon: String,
    pub protein: String, // 翻译的氨基酸序列（不含终止符）
}

/// 简化版密码子翻译（复用已有 codon_to_aa，处理 &[u8; 3]）
fn translate_codon_simple(codon: &[u8]) -> char {
    let n = |i: usize| codon[i].to_ascii_uppercase();
    // 终止密码子
    if matches!(
        (n(0), n(1), n(2)),
        (b'T', b'A', b'A')
            | (b'T', b'A', b'G')
            | (b'T', b'G', b'A')
            | (b'U', b'A', b'A')
            | (b'U', b'A', b'G')
            | (b'U', b'G', b'A')
    ) {
        return '*';
    }
    // 起始密码子 ATG/AUG → M
    if matches!((n(0), n(1), n(2)), (b'A', b'T', b'G') | (b'A', b'U', b'G')) {
        return 'M';
    }
    // 第一位
    let first = match n(0) {
        b'T' | b'U' => 0,
        b'C' => 1,
        b'A' => 2,
        b'G' => 3,
        _ => return 'X',
    };
    let second = match n(1) {
        b'T' | b'U' => 0,
        b'C' => 1,
        b'A' => 2,
        b'G' => 3,
        _ => return 'X',
    };
    let third = match n(2) {
        b'T' | b'U' => 0,
        b'C' => 1,
        b'A' => 2,
        b'G' => 3,
        _ => return 'X',
    };
    // 标准密码子表（T/U 统一处理）
    const TABLE: [char; 64] = [
        'F', 'F', 'L', 'L', 'S', 'S', 'S', 'S', 'Y', 'Y', '*', '*', 'C', 'C', '*', 'W', 'L', 'L',
        'L', 'L', 'P', 'P', 'P', 'P', 'H', 'H', 'Q', 'Q', 'R', 'R', 'R', 'R', 'I', 'I', 'I', 'M',
        'T', 'T', 'T', 'T', 'N', 'N', 'K', 'K', 'S', 'S', 'R', 'R', 'V', 'V', 'V', 'V', 'A', 'A',
        'A', 'A', 'D', 'D', 'E', 'E', 'G', 'G', 'G', 'G',
    ];
    TABLE[first * 16 + second * 4 + third]
}

/// 预测 DNA 序列中的开放阅读框（ORF）
/// 在 6 个读码框中查找 ATG..Stop，返回所有 ORF（按长度降序）
/// min_length 为最小 ORF 长度（含终止密码子，单位：核苷酸）
#[napi]
pub fn find_orfs(sequence: String, min_length: i64) -> Vec<OrfResult> {
    let seq: Vec<u8> = sequence
        .bytes()
        .map(|b| {
            let u = b.to_ascii_uppercase();
            if u == b'U' {
                b'T'
            } else {
                u
            }
        })
        .filter(|b| matches!(b, b'A' | b'T' | b'C' | b'G' | b'N'))
        .collect();

    let min_len = if min_length > 0 {
        min_length as usize
    } else {
        30
    };
    let mut results: Vec<OrfResult> = Vec::new();

    // 反向互补序列
    let rc_seq: Vec<u8> = seq
        .iter()
        .rev()
        .map(|&b| match b {
            b'A' => b'T',
            b'T' => b'A',
            b'C' => b'G',
            b'G' => b'C',
            _ => b'N',
        })
        .collect();

    // 在正链和反链的 3 个读码框中搜索
    let strands: [(&[u8], &str); 2] = [(&seq, "+"), (&rc_seq, "-")];
    for (s, strand_label) in strands.iter() {
        for frame in 0..3 {
            let mut i = frame;
            while i + 3 <= s.len() {
                let codon = &s[i..i + 3];
                if codon == b"ATG" {
                    // 找到起始密码子，向后查找终止
                    let mut j = i + 3;
                    let mut found_stop = false;
                    while j + 3 <= s.len() {
                        let c = &s[j..j + 3];
                        if c == b"TAA" || c == b"TAG" || c == b"TGA" {
                            found_stop = true;
                            break;
                        }
                        j += 3;
                    }
                    if found_stop && (j + 3 - i) >= min_len {
                        // 翻译
                        let orf_len = j + 3 - i;
                        let mut protein = String::with_capacity(orf_len / 3);
                        let mut k = i;
                        while k < j {
                            let c = &s[k..k + 3];
                            protein.push(translate_codon_simple(c) as char);
                            k += 3;
                        }
                        results.push(OrfResult {
                            start: if *strand_label == "+" {
                                i as i64
                            } else {
                                (s.len() - j - 3) as i64
                            },
                            end: if *strand_label == "+" {
                                (j + 3) as i64
                            } else {
                                (s.len() - i) as i64
                            },
                            length: orf_len as i64,
                            strand: strand_label.to_string(),
                            frame: frame as i64,
                            start_codon: "ATG".to_string(),
                            stop_codon: String::from_utf8_lossy(&s[j..j + 3]).to_string(),
                            protein,
                        });
                    }
                    // 跳过此 ATG（避免重叠 ORF）
                    i = j + 3;
                } else {
                    i += 3;
                }
            }
        }
    }

    // 按长度降序
    results.sort_by(|a, b| b.length.cmp(&a.length));
    results
}

// ============ Token 估算（与 src/main/services/compaction.ts 的 estimateTokens 等价） ============

/// 判断一个 Unicode 码点是否落在 TS 版 isCjk 的 8 个 CJK 范围内
#[inline]
fn is_cjk_ts(cp: u32) -> bool {
    (0x3000..=0x303F).contains(&cp)
        || (0x3040..=0x309F).contains(&cp)
        || (0x30A0..=0x30FF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFF00..=0xFFEF).contains(&cp)
}

/// 估算文本 token 数：全 ASCII 快速路径 len/4 向上取整；
/// 否则 CJK 1 char = 1 token，其余 4 char = 1 token（向上取整）。
/// 与 src/main/services/compaction.ts::estimateTokens 等价（行为对齐 TS 的 for..of 码点语义）
#[napi]
pub fn estimate_tokens(text: String) -> u32 {
    if text.is_empty() {
        return 0;
    }
    // 快速路径：等价于 /^[\x00-\x7F]*$/
    // Rust String 保证 UTF-8，字节 <0x80 ⟺ 完整 ASCII 标量值（不会有部分序列误判）
    if text.bytes().all(|b| b < 0x80) {
        return ((text.len() as u64 + 3) / 4) as u32;
    }
    let mut cjk: u64 = 0;
    let mut other: u64 = 0;
    for c in text.chars() {
        if is_cjk_ts(c as u32) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (cjk + (other + 3) / 4) as u32
}

#[cfg(test)]
mod estimate_tokens_tests {
    use super::estimate_tokens;

    #[test]
    fn ascii_fast_path() {
        assert_eq!(estimate_tokens("hello world".into()), 3); // ceil(11/4)
        assert_eq!(estimate_tokens("abcd".into()), 1);
        assert_eq!(estimate_tokens("abcde".into()), 2); // ceil(5/4)
    }

    #[test]
    fn pure_cjk() {
        assert_eq!(estimate_tokens("你好世界".into()), 4);
    }

    #[test]
    fn mixed_cjk_ascii() {
        assert_eq!(estimate_tokens("你好 world".into()), 4); // 2 CJK + (6+3)/4
    }

    #[test]
    fn empty() {
        assert_eq!(estimate_tokens("".into()), 0);
    }

    #[test]
    fn emoji_counted_as_other() {
        // 🐍 U+1F40D 码点级 1 字符，不在 CJK 表，走 other
        assert_eq!(estimate_tokens("🐍🐍🐍🐍".into()), 1);
        assert_eq!(estimate_tokens("🐍🐍🐍🐍🐍".into()), 2); // ceil(5/4)
    }
}

// ============================================================================
// TF-IDF 索引（常驻内存，供 memory-store 检索下沉）
// ============================================================================

// 与 src/main/services/memory-store.ts 的 buildTfidfIndex + tfidf + cosine 等价。
// 设计：
//   - JS 端用现有 tokenise/memoryTokens 提取 tokens（含 CJK bigram / 别名 / 实体）
//   - JS 每次检索调用 tfidf_ensure(corpusKey, docsJson)，若 key 变化则 Rust 重建索引
//   - JS 再调 tfidf_score(qTokensJson, k) 拿基础 TF-IDF 分
//   - effectiveness / meta / graph boost 留在 JS（依赖 frontmatter，不跨边界）
//
// 性能：N=500 memories × 平均 200 tokens = 100K token 的索引重建 <10ms（Rust HashMap）
// 检索：单次 cosine 全量扫 <1ms

/// 单条文档输入（来自 JS）
#[derive(serde::Deserialize)]
struct TfidfDoc {
    id: String,
    tokens: Vec<String>,
}

/// 内部索引项：id + 预计算 tfidf 向量
struct TfidfEntry {
    id: String,
    vec: HashMap<String, f64>,
}

/// 全局 TF-IDF 索引（单例常驻内存）
struct TfidfIndex {
    /// 缓存 key：JS 端传入的 ids+mtime，变化即重建
    key: String,
    /// 文档总数
    n: usize,
    /// document frequency: token -> 出现过该 token 的文档数
    df: HashMap<String, u32>,
    /// 每个文档的预计算 tfidf 向量
    entries: Vec<TfidfEntry>,
}

static TFIDF_INDEX: OnceLock<Mutex<Option<TfidfIndex>>> = OnceLock::new();

fn tfidf_index_slot() -> &'static Mutex<Option<TfidfIndex>> {
    TFIDF_INDEX.get_or_init(|| Mutex::new(None))
}

/// 计算单文档的 tfidf 向量
fn tfidf_vec(
    tf: &HashMap<String, u32>,
    df: &HashMap<String, u32>,
    n: usize,
) -> HashMap<String, f64> {
    let n_f = n as f64;
    let mut vec = HashMap::with_capacity(tf.len());
    for (t, &f) in tf.iter() {
        let df_v = *df.get(t).unwrap_or(&0) as f64;
        // 与 TS 版一致：idf = ln(1 + n / (1 + df))
        let idf = (1.0 + n_f / (1.0 + df_v)).ln();
        vec.insert(t.clone(), (f as f64) * idf);
    }
    vec
}

/// cosine 相似度（与 TS 版一致：a·b / sqrt(|a|²·|b|²)，任一向量零返回 0）
fn cosine_sim(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    let mut dot = 0.0_f64;
    let mut na = 0.0_f64;
    let mut nb = 0.0_f64;
    for (k, &v) in a.iter() {
        na += v * v;
        if let Some(&bv) = b.get(k) {
            dot += v * bv;
        }
    }
    for &v in b.values() {
        nb += v * v;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb).sqrt()
}

/// 确保索引与给定 corpusKey 一致：key 变化则重建，否则复用
/// 返回重建次数（0 = 复用，1 = 重建）
#[napi]
pub fn tfidf_ensure(corpus_key: String, docs_json: String) -> u32 {
    let docs: Vec<TfidfDoc> = match serde_json::from_str(&docs_json) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[bioio::tfidf_ensure] JSON parse failed: {}", e);
            return 0;
        }
    };

    let slot = tfidf_index_slot();
    let mut guard = match slot.lock() {
        Ok(g) => g,
        Err(_) => return 0, // poisoned，放弃
    };

    // key 相同则复用
    if let Some(ref idx) = *guard {
        if idx.key == corpus_key {
            return 0;
        }
    }

    // 重建
    let n = docs.len();
    let mut df: HashMap<String, u32> = HashMap::new();
    let mut tf_list: Vec<(String, HashMap<String, u32>)> = Vec::with_capacity(n);

    for doc in docs {
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in &doc.tokens {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        // df: 每个 token 在当前文档出现则 +1
        for t in tf.keys() {
            *df.entry(t.clone()).or_insert(0) += 1;
        }
        tf_list.push((doc.id, tf));
    }

    let entries: Vec<TfidfEntry> = tf_list
        .into_iter()
        .map(|(id, tf)| TfidfEntry {
            id,
            vec: tfidf_vec(&tf, &df, n),
        })
        .collect();

    *guard = Some(TfidfIndex {
        key: corpus_key,
        n,
        df,
        entries,
    });
    1
}

/// TF-IDF 检索：给 query tokens + top-k，返回基础相似度分数列表
/// 返回 JSON: [{id, base}]
#[napi]
pub fn tfidf_score(q_tokens_json: String, k: u32) -> String {
    let q_tokens: Vec<String> = match serde_json::from_str(&q_tokens_json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[bioio::tfidf_score] JSON parse failed: {}", e);
            return "[]".to_string();
        }
    };

    let slot = tfidf_index_slot();
    let guard = match slot.lock() {
        Ok(g) => g,
        Err(_) => return "[]".to_string(),
    };
    let idx = match guard.as_ref() {
        Some(i) => i,
        None => return "[]".to_string(), // 未 ensure 过
    };

    // query 的 tf
    let mut q_tf: HashMap<String, u32> = HashMap::new();
    for t in &q_tokens {
        *q_tf.entry(t.clone()).or_insert(0) += 1;
    }
    // query 的 tfidf（用全局 df 和 n）
    let q_vec = tfidf_vec(&q_tf, &idx.df, idx.n);

    // 全量扫，取 top-k
    let mut scored: Vec<(String, f64)> = idx
        .entries
        .iter()
        .map(|e| (e.id.clone(), cosine_sim(&q_vec, &e.vec)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k as usize);

    // 返回 JSON
    let items: Vec<serde_json::Value> = scored
        .into_iter()
        .map(|(id, base)| serde_json::json!({ "id": id, "base": base }))
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// 清空 TF-IDF 索引（通常由 JS 端在 invalidateMemCache 时调用）
#[napi]
pub fn tfidf_clear() {
    if let Ok(mut guard) = tfidf_index_slot().lock() {
        *guard = None;
    }
}

/// 获取当前索引信息（调试/测试用）
#[napi]
pub fn tfidf_index_info() -> String {
    let slot = tfidf_index_slot();
    let guard = match slot.lock() {
        Ok(g) => g,
        Err(_) => return r#"{"ok":false,"reason":"poisoned"}"#.to_string(),
    };
    match guard.as_ref() {
        Some(idx) => serde_json::json!({
            "ok": true,
            "key": idx.key,
            "n": idx.n,
            "dfSize": idx.df.len(),
            "entriesLen": idx.entries.len(),
        })
        .to_string(),
        None => r#"{"ok":false,"reason":"empty"}"#.to_string(),
    }
}

#[cfg(test)]
mod tfidf_tests {
    use super::*;

    fn build_sample() -> String {
        serde_json::to_string(&vec![
            serde_json::json!({"id": "m1", "tokens": ["samtools", "variant", "calling"]}),
            serde_json::json!({"id": "m2", "tokens": ["bcftools", "filter", "variant"]}),
            serde_json::json!({"id": "m3", "tokens": ["fastp", "qc", "过滤", "质控"]}),
        ])
        .unwrap()
    }

    // 注意：测试默认并行执行，而 TFIDF_INDEX 是全局单例。
    // 为避免测试之间互相污染，把所有对全局索引的断言收敛到一个 test fn 中。
    #[test]
    fn tfidf_index_lifecycle() {
        // 1. 初始为空（或上一个测试残留，先 clear 保证起点干净）
        tfidf_clear();
        let info0 = tfidf_index_info();
        assert!(info0.contains("\"ok\":false"), "初始应为空: {}", info0);

        // 2. ensure 第一次重建
        let docs = build_sample();
        let rebuild = tfidf_ensure("k1".to_string(), docs.clone());
        assert_eq!(rebuild, 1, "首次 ensure 应重建");

        // 3. 同 key 重复 ensure 不重建
        let rebuild = tfidf_ensure("k1".to_string(), docs.clone());
        assert_eq!(rebuild, 0, "同 key 应复用");

        // 4. 换 key 重建
        let rebuild = tfidf_ensure("k2".to_string(), docs.clone());
        assert_eq!(rebuild, 1, "换 key 应重建");

        // 5. score: query "variant" 应命中 m1 m2
        let result = tfidf_score(r#"["variant"]"#.to_string(), 10);
        let v: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(
            v.len() >= 2,
            "variant 应命中至少 2 条记忆, 实际: {}",
            result
        );
        let ids: Vec<&str> = v.iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"m1"));
        assert!(ids.contains(&"m2"));

        // 6. score: query 无命中 token → 返回 []
        let result = tfidf_score(r#"["nonexistent_tool_xyz_999"]"#.to_string(), 10);
        assert_eq!(result, "[]", "无命中 token 应返回空数组");

        // 7. clear 后 score 应返回 []
        tfidf_clear();
        let result = tfidf_score(r#"["variant"]"#.to_string(), 10);
        assert_eq!(result, "[]");

        // 8. info 在 clear 后反映空状态
        let info_final = tfidf_index_info();
        assert!(info_final.contains("\"ok\":false"));
    }

    // 纯函数 cosine/tfidf_vec 不依赖全局状态，可以并行跑
    #[test]
    fn cosine_zero_vec_returns_zero() {
        let a: HashMap<String, f64> = HashMap::new();
        let mut b: HashMap<String, f64> = HashMap::new();
        b.insert("x".to_string(), 1.0);
        assert_eq!(cosine_sim(&a, &b), 0.0);
        assert_eq!(cosine_sim(&b, &a), 0.0);
        assert_eq!(cosine_sim(&a, &a), 0.0);
    }

    #[test]
    fn cosine_identical_vecs() {
        let mut a: HashMap<String, f64> = HashMap::new();
        a.insert("x".to_string(), 1.0);
        a.insert("y".to_string(), 2.0);
        let s = cosine_sim(&a, &a);
        assert!(
            (s - 1.0).abs() < 1e-10,
            "identical vecs cosine 应为 1, 实际 {}",
            s
        );
    }

    #[test]
    fn tfidf_vec_basic() {
        let mut tf: HashMap<String, u32> = HashMap::new();
        tf.insert("foo".to_string(), 2);
        let mut df: HashMap<String, u32> = HashMap::new();
        df.insert("foo".to_string(), 1);
        let v = tfidf_vec(&tf, &df, 10);
        // idf = ln(1 + 10/(1+1)) = ln(6) ≈ 1.7918
        // tfidf = 2 * 1.7918 ≈ 3.5835
        let expected = 2.0 * (1.0_f64 + 10.0 / 2.0).ln();
        let got = v.get("foo").copied().unwrap_or(0.0);
        assert!((got - expected).abs() < 1e-9, "got={} expected={}", got,);
    }
}

// ============ Dense vector cosine（embedding 用） ============

/// 计算两个 dense 向量的 cosine 相似度（f64 精度）
/// 与 JS 版 embeddingCosine 等价：长度不等或零向量返回 0
#[napi]
pub fn embedding_cosine(a: Vec<f64>, b: Vec<f64>) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// 批量 cosine：query 对 N 个候选向量，返回 Vec<f64>
/// 用于 embedding ANN 失效时的线性扫描（N×D 百万级浮点运算，Rust 比 V8 快 ~10x）
#[napi]
pub fn embedding_cosine_batch(query: Vec<f64>, candidates: Vec<Vec<f64>>) -> Vec<f64> {
    candidates
        .iter()
        .map(|c| {
            if c.len() != query.len() || c.is_empty() {
                return 0.0;
            }
            let mut dot = 0.0_f64;
            let mut norm_q = 0.0_f64;
            let mut norm_c = 0.0_f64;
            for i in 0..query.len() {
                dot += query[i] * c[i];
                norm_q += query[i] * query[i];
                norm_c += c[i] * c[i];
            }
            let denom = norm_q.sqrt() * norm_c.sqrt();
            if denom == 0.0 {
                0.0
            } else {
                dot / denom
            }
        })
        .collect()
}

#[cfg(test)]
mod embedding_cosine_tests {
    use super::*;

    #[test]
    fn cosine_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let s = embedding_cosine(a.clone(), a);
        assert!((s - 1.0).abs() < 1e-10);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(embedding_cosine(a, b).abs() < 1e-10);
    }

    #[test]
    fn cosine_opposite() {
        let a = vec![1.0, 2.0];
        let b = vec![-1.0, -2.0];
        let s = embedding_cosine(a, b);
        assert!((s + 1.0).abs() < 1e-10);
    }

    #[test]
    fn cosine_length_mismatch() {
        assert_eq!(embedding_cosine(vec![1.0, 2.0], vec![1.0, 2.0, 3.0]), 0.0);
    }

    #[test]
    fn cosine_empty() {
        assert_eq!(embedding_cosine(vec![], vec![]), 0.0);
    }

    #[test]
    fn cosine_zero_vector() {
        assert_eq!(embedding_cosine(vec![0.0, 0.0], vec![1.0, 2.0]), 0.0);
    }

    #[test]
    fn cosine_batch_basic() {
        let q = vec![1.0, 0.0];
        let candidates = vec![
            vec![1.0, 0.0],  // cos=1
            vec![0.0, 1.0],  // cos=0
            vec![-1.0, 0.0], // cos=-1
        ];
        let scores = embedding_cosine_batch(q, candidates);
        assert_eq!(scores.len(), 3);
        assert!((scores[0] - 1.0).abs() < 1e-10);
        assert!(scores[1].abs() < 1e-10);
        assert!((scores[2] + 1.0).abs() < 1e-10);
    }
}
