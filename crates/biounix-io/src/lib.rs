// BioUnix 文件 I/O 生信计算模块（biounix-io）
// 由 Rust 编写，通过 napi-rs 暴露给 Node.js / Electron 主进程
// 专注于 FASTA/FASTQ/BAM/VCF/BCF/GFF/BED 文件解析统计
#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};

use noodles::bam;
use noodles::bcf;
use noodles::fasta;
use noodles::fastq;
use noodles::gff;
use noodles::sam::alignment::record::Flags as SamFlags;
use noodles::vcf;
use noodles::vcf::variant::record::AlternateBases as _;
use noodles::vcf::variant::record::Filters as _;
use noodles::vcf::variant::record::Ids as _;
use noodles::vcf::variant::record::Info as _;

// ============ 内联辅助：GC 含量计算（read_fasta_stats 使用，避免跨 crate 依赖） ============

/// 计算序列的 GC 含量（百分比，0-100）
fn gc_content_inline(sequence: &str) -> f64 {
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

#[napi(object)]
pub struct FastaRecord {
    pub id: String,
    pub description: String,
    pub length: i64,
    pub gc: f64,
}

/// 读取 FASTA 文件并统计每条序列（长度、GC 含量）
/// 使用 noodles 解析，支持规范 FASTA 格式
#[napi]
pub fn read_fasta_stats(path: String) -> napi::Result<Vec<FastaRecord>> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = fasta::io::Reader::new(BufReader::new(file));
    let mut records = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;
        let seq_bytes: &[u8] = record.sequence().as_ref();
        let seq_str = String::from_utf8_lossy(seq_bytes).to_string();
        let gc = gc_content_inline(&seq_str);
        let description = record
            .description()
            .map(|d| d.to_string())
            .unwrap_or_default();
        records.push(FastaRecord {
            id: String::from_utf8_lossy(record.name()).to_string(),
            description,
            length: seq_bytes.len() as i64,
            gc,
        });
    }
    Ok(records)
}

#[napi(object)]
pub struct FastqStats {
    pub read_count: i64,
    pub total_bases: i64,
    pub avg_quality: f64,
    pub q20_percent: f64,
    pub q30_percent: f64,
    pub gc_percent: f64,
}

/// 读取 FASTQ 文件并统计质量（Q20/Q30、GC%、平均质量）
/// 使用 noodles 解析，支持规范 FASTQ 格式
#[napi]
pub fn fastq_quality_stats(path: String) -> napi::Result<FastqStats> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = fastq::io::Reader::new(BufReader::new(file));
    let mut read_count: u64 = 0;
    let mut total_bases: u64 = 0;
    let mut total_quality: u64 = 0;
    let mut q20: u64 = 0;
    let mut q30: u64 = 0;
    let mut gc: u64 = 0;
    let mut at: u64 = 0;

    for result in reader.records() {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;
        // 统计序列碱基（GC/AT）
        for &b in record.sequence() {
            match b.to_ascii_uppercase() {
                b'G' | b'C' => gc += 1,
                b'A' | b'T' => at += 1,
                _ => {}
            }
        }
        let seq_len = record.sequence().len() as u64;
        total_bases += seq_len;
        // 统计质量值（Phred+33）
        for &q_byte in record.quality_scores() {
            let q = (q_byte - 33) as u64;
            total_quality += q;
            if q >= 20 {
                q20 += 1;
            }
            if q >= 30 {
                q30 += 1;
            }
        }
        read_count += 1;
    }

    let avg_quality = if total_bases == 0 {
        0.0
    } else {
        total_quality as f64 / total_bases as f64
    };
    let q20_percent = if total_bases == 0 {
        0.0
    } else {
        (q20 as f64 / total_bases as f64) * 100.0
    };
    let q30_percent = if total_bases == 0 {
        0.0
    } else {
        (q30 as f64 / total_bases as f64) * 100.0
    };
    let gc_percent = if (gc + at) == 0 {
        0.0
    } else {
        (gc as f64 / (gc + at) as f64) * 100.0
    };

    Ok(FastqStats {
        read_count: read_count as i64,
        total_bases: total_bases as i64,
        avg_quality,
        q20_percent,
        q30_percent,
        gc_percent,
    })
}

#[napi(object)]
pub struct BamStats {
    pub read_count: i64,
    pub mapped_count: i64,
    pub unmapped_count: i64,
    pub paired_count: i64,
    pub proper_pair_count: i64,
    pub duplicate_count: i64,
    pub total_bases: i64,
    pub avg_mapq: f64,
    pub avg_length: f64,
    pub gc_percent: f64,
}

/// 读取 BAM/SAM 文件并统计比对信息
/// 使用 noodles-bam 解析（BAM 自动 BGZF 解压；SAM 为文本）
#[napi]
pub fn read_bam_stats(path: String) -> napi::Result<BamStats> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = bam::io::Reader::new(file);
    let _header = reader.read_header().map_err(|e| {
        napi::Error::new(
            napi::Status::GenericFailure,
            format!("读取 BAM header 失败: {}", e),
        )
    })?;

    let mut read_count: u64 = 0;
    let mut mapped_count: u64 = 0;
    let mut unmapped_count: u64 = 0;
    let mut paired_count: u64 = 0;
    let mut proper_pair_count: u64 = 0;
    let mut duplicate_count: u64 = 0;
    let mut total_bases: u64 = 0;
    let mut total_mapq: u64 = 0;
    let mut total_len: u64 = 0;
    let mut gc: u64 = 0;
    let mut at: u64 = 0;

    for result in reader.records() {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;
        let flags = SamFlags::from(record.flags());
        let is_unmapped = flags.contains(SamFlags::UNMAPPED);
        let is_paired = flags.contains(SamFlags::SEGMENTED);
        let is_proper = flags.contains(SamFlags::PROPERLY_SEGMENTED);
        let is_dup = flags.contains(SamFlags::DUPLICATE);

        if is_unmapped {
            unmapped_count += 1;
        } else {
            mapped_count += 1;
        }
        if is_paired {
            paired_count += 1;
        }
        if is_proper {
            proper_pair_count += 1;
        }
        if is_dup {
            duplicate_count += 1;
        }

        // 序列长度与 GC
        let seq = record.sequence();
        let seq_len = seq.len() as u64;
        total_bases += seq_len;
        total_len += seq_len;
        for &b in seq.as_ref() {
            match b.to_ascii_uppercase() {
                b'G' | b'C' => gc += 1,
                b'A' | b'T' => at += 1,
                _ => {}
            }
        }

        // MapQ（255 表示 missing）
        if let Some(mq) = record.mapping_quality() {
            let q = u8::from(mq) as u64;
            if q != 255 {
                total_mapq += q;
            }
        }

        read_count += 1;
    }

    let avg_mapq = if mapped_count == 0 {
        0.0
    } else {
        total_mapq as f64 / mapped_count as f64
    };
    let avg_length = if read_count == 0 {
        0.0
    } else {
        total_len as f64 / read_count as f64
    };
    let gc_percent = if (gc + at) == 0 {
        0.0
    } else {
        (gc as f64 / (gc + at) as f64) * 100.0
    };

    Ok(BamStats {
        read_count: read_count as i64,
        mapped_count: mapped_count as i64,
        unmapped_count: unmapped_count as i64,
        paired_count: paired_count as i64,
        proper_pair_count: proper_pair_count as i64,
        duplicate_count: duplicate_count as i64,
        total_bases: total_bases as i64,
        avg_mapq,
        avg_length,
        gc_percent,
    })
}

/// VCF/BCF 变异统计
#[napi(object)]
pub struct VcfStats {
    pub variant_count: i64,
    pub snp_count: i64,
    pub indel_count: i64,
    pub multi_allelic_count: i64,
    pub sample_count: i64,
    pub pass_count: i64,
    pub avg_qual: f64,
    pub chromosome_count: i64,
}

/// 统计单个 VCF RecordBuf（VCF 和 BCF 共用）
fn stats_vcf_record(
    record: &vcf::variant::RecordBuf,
    snp_count: &mut u64,
    indel_count: &mut u64,
    multi_allelic_count: &mut u64,
    pass_count: &mut u64,
    total_qual: &mut f64,
    qual_n: &mut u64,
) {
    let ref_bases = record.reference_bases();
    let alt_bases = record.alternate_bases();
    let alt_len = alt_bases.len();

    // SNV vs indel：ref 和所有 alt 均为单碱基 → SNV
    let ref_is_single = ref_bases.len() == 1;
    let all_alt_single = alt_bases
        .iter()
        .all(|result| result.map(|a| a.len() == 1).unwrap_or(false));
    if ref_is_single && all_alt_single {
        *snp_count += 1;
    } else {
        *indel_count += 1;
    }
    if alt_len > 1 {
        *multi_allelic_count += 1;
    }

    // PASS filter
    if record.filters().is_pass() {
        *pass_count += 1;
    }

    // QUAL（quality_score 返回 Option<f32>，直接用）
    if let Some(qual) = record.quality_score() {
        *total_qual += qual as f64;
        *qual_n += 1;
    }
}

/// 读取 VCF 文件并统计变异信息
/// 使用 noodles-vcf 解析（文本格式）
#[napi]
pub fn read_vcf_stats(path: String) -> napi::Result<VcfStats> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = vcf::io::Reader::new(BufReader::new(file));
    let header = reader.read_header().map_err(|e| {
        napi::Error::new(
            napi::Status::GenericFailure,
            format!("读取 VCF header 失败: {}", e),
        )
    })?;

    let sample_count = header.sample_names().len() as i64;
    let mut chromosome_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut variant_count: u64 = 0;
    let mut snp_count: u64 = 0;
    let mut indel_count: u64 = 0;
    let mut multi_allelic_count: u64 = 0;
    let mut pass_count: u64 = 0;
    let mut total_qual: f64 = 0.0;
    let mut qual_n: u64 = 0;

    for result in reader.records() {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;
        let record_buf = vcf::variant::RecordBuf::try_from_variant_record(&header, &record)
            .map_err(|e| {
                napi::Error::new(napi::Status::GenericFailure, format!("记录转换错误: {}", e))
            })?;

        chromosome_set.insert(record_buf.reference_sequence_name().to_string());
        stats_vcf_record(
            &record_buf,
            &mut snp_count,
            &mut indel_count,
            &mut multi_allelic_count,
            &mut pass_count,
            &mut total_qual,
            &mut qual_n,
        );
        variant_count += 1;
    }

    let avg_qual = if qual_n == 0 {
        0.0
    } else {
        total_qual / qual_n as f64
    };

    Ok(VcfStats {
        variant_count: variant_count as i64,
        snp_count: snp_count as i64,
        indel_count: indel_count as i64,
        multi_allelic_count: multi_allelic_count as i64,
        sample_count,
        pass_count: pass_count as i64,
        avg_qual,
        chromosome_count: chromosome_set.len() as i64,
    })
}

/// 读取 BCF 文件并统计变异信息
/// 使用 noodles-bcf 解析（二进制格式，自动 BGZF 解压）
#[napi]
pub fn read_bcf_stats(path: String) -> napi::Result<VcfStats> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = bcf::io::Reader::new(file);
    let header = reader.read_header().map_err(|e| {
        napi::Error::new(
            napi::Status::GenericFailure,
            format!("读取 BCF header 失败: {}", e),
        )
    })?;

    let sample_count = header.sample_names().len() as i64;
    let mut chromosome_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut variant_count: u64 = 0;
    let mut snp_count: u64 = 0;
    let mut indel_count: u64 = 0;
    let mut multi_allelic_count: u64 = 0;
    let mut pass_count: u64 = 0;
    let mut total_qual: f64 = 0.0;
    let mut qual_n: u64 = 0;

    for result in reader.record_bufs(&header) {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;

        chromosome_set.insert(record.reference_sequence_name().to_string());
        stats_vcf_record(
            &record,
            &mut snp_count,
            &mut indel_count,
            &mut multi_allelic_count,
            &mut pass_count,
            &mut total_qual,
            &mut qual_n,
        );
        variant_count += 1;
    }

    let avg_qual = if qual_n == 0 {
        0.0
    } else {
        total_qual / qual_n as f64
    };

    Ok(VcfStats {
        variant_count: variant_count as i64,
        snp_count: snp_count as i64,
        indel_count: indel_count as i64,
        multi_allelic_count: multi_allelic_count as i64,
        sample_count,
        pass_count: pass_count as i64,
        avg_qual,
        chromosome_count: chromosome_set.len() as i64,
    })
}

/// GFF/GTF feature 类型计数
#[napi(object)]
pub struct GffFeatureCount {
    pub feature_type: String,
    pub count: i64,
}

/// GFF/GTF 文件统计
#[napi(object)]
pub struct GffStats {
    pub feature_count: i64,
    pub source_count: i64,
    pub chromosome_count: i64,
    pub feature_types: Vec<GffFeatureCount>,
    pub strand_plus: i64,
    pub strand_minus: i64,
    pub strand_none: i64,
    pub total_span: i64,
}

/// 读取 GFF/GTF 文件并统计 feature 信息
/// 使用 noodles-gff 解析（支持 GFF3 和 GTF 格式）
#[napi]
pub fn read_gff_stats(path: String) -> napi::Result<GffStats> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = gff::io::Reader::new(BufReader::new(file));

    let mut feature_count: u64 = 0;
    let mut source_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut chrom_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut feature_map: HashMap<String, u64> = HashMap::new();
    let mut strand_plus: u64 = 0;
    let mut strand_minus: u64 = 0;
    let mut strand_none: u64 = 0;
    let mut total_span: u64 = 0;

    for result in reader.record_bufs() {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;

        let chrom = String::from_utf8_lossy(record.reference_sequence_name()).to_string();
        chrom_set.insert(chrom);
        let source = String::from_utf8_lossy(record.source()).to_string();
        source_set.insert(source);
        let ftype = String::from_utf8_lossy(record.ty()).to_string();
        *feature_map.entry(ftype.clone()).or_insert(0) += 1;

        // start/end → span（RecordBuf 直接返回 Position，非 Result）
        let start = record.start().get() as u64;
        let end = record.end().get() as u64;
        if end > start {
            total_span += end - start + 1;
        }

        // strand（RecordBuf 直接返回 Strand，非 Result）
        use noodles::gff::feature::record::Strand as GffStrand;
        match record.strand() {
            GffStrand::Forward => strand_plus += 1,
            GffStrand::Reverse => strand_minus += 1,
            _ => strand_none += 1,
        }

        feature_count += 1;
    }

    let mut feature_types: Vec<GffFeatureCount> = feature_map
        .into_iter()
        .map(|(t, c)| GffFeatureCount {
            feature_type: t,
            count: c as i64,
        })
        .collect();
    feature_types.sort_by(|a, b| b.count.cmp(&a.count));

    Ok(GffStats {
        feature_count: feature_count as i64,
        source_count: source_set.len() as i64,
        chromosome_count: chrom_set.len() as i64,
        feature_types,
        strand_plus: strand_plus as i64,
        strand_minus: strand_minus as i64,
        strand_none: strand_none as i64,
        total_span: total_span as i64,
    })
}

/// BED 染色体分布
#[napi(object)]
pub struct BedChromCount {
    pub chromosome: String,
    pub count: i64,
    pub total_span: i64,
}

/// BED 文件统计
#[napi(object)]
pub struct BedStats {
    pub feature_count: i64,
    pub chromosome_count: i64,
    pub total_span: i64,
    pub min_length: i64,
    pub max_length: i64,
    pub avg_length: f64,
    pub chromosomes: Vec<BedChromCount>,
}

/// 读取 BED 文件并统计 feature 信息
/// 纯文本解析，支持 BED3/BED6/BED12 格式
#[napi]
pub fn read_bed_stats(path: String) -> napi::Result<BedStats> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let reader = BufReader::new(file);

    let mut feature_count: u64 = 0;
    let mut chrom_map: HashMap<String, (u64, u64)> = HashMap::new(); // chrom → (count, span)
    let mut total_span: u64 = 0;
    let mut min_len: u64 = u64::MAX;
    let mut max_len: u64 = 0;
    let mut total_len: u64 = 0;

    for line in reader.lines() {
        let line = line.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("读取错误: {}", e))
        })?;
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("track")
            || line.starts_with("browser")
        {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let chrom = fields[0].to_string();
        let start: u64 = fields[1].parse().unwrap_or(0);
        let end: u64 = fields[2].parse().unwrap_or(0);
        let len = if end > start { end - start } else { 0 };

        let entry = chrom_map.entry(chrom.clone()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += len;

        total_span += len;
        total_len += len;
        if len < min_len {
            min_len = len;
        }
        if len > max_len {
            max_len = len;
        }
        feature_count += 1;
    }

    let mut chromosomes: Vec<BedChromCount> = chrom_map
        .into_iter()
        .map(|(chromosome, (count, span))| BedChromCount {
            chromosome,
            count: count as i64,
            total_span: span as i64,
        })
        .collect();
    chromosomes.sort_by(|a, b| b.count.cmp(&a.count));

    let avg_length = if feature_count == 0 {
        0.0
    } else {
        total_len as f64 / feature_count as f64
    };
    let min_length = if feature_count == 0 {
        0
    } else {
        min_len as i64
    };

    Ok(BedStats {
        feature_count: feature_count as i64,
        chromosome_count: chromosomes.len() as i64,
        total_span: total_span as i64,
        min_length,
        max_length: max_len as i64,
        avg_length,
        chromosomes,
    })
}

// ============================================================
// VCF 变异过滤提取（按 QUAL/PASS/区间/染色体）
// ============================================================

/// VCF 过滤后的变异记录
#[napi(object)]
pub struct VcfVariant {
    pub chromosome: String,
    pub position: i64, // 1-based
    pub id: String,
    pub reference: String,
    pub alternate: String,
    pub quality: f64,
    pub filter: String,
    pub info: String,
}

/// VCF 过滤结果
#[napi(object)]
pub struct VcfFilterResult {
    pub total_count: i64,
    pub filtered_count: i64,
    pub variants: Vec<VcfVariant>,
}

/// 过滤 VCF 文件，按条件提取变异记录
/// - min_quality：QUAL 最小值（NaN 表示不限制）
/// - pass_only：true 表示仅保留 PASS 过滤的变异
/// - chromosome：指定染色体（None/空表示不限制）
/// - region_start/region_end：区间限制（1-based，0 表示不限制）
/// - max_variants：最多返回的变异数（0 表示不限制）
#[napi]
pub fn filter_vcf(
    path: String,
    min_quality: f64,
    pass_only: bool,
    chromosome: String,
    region_start: i64,
    region_end: i64,
    max_variants: i64,
) -> napi::Result<VcfFilterResult> {
    let file = File::open(&path).map_err(|e| {
        napi::Error::new(napi::Status::GenericFailure, format!("无法打开文件: {}", e))
    })?;
    let mut reader = vcf::io::Reader::new(BufReader::new(file));
    let header = reader.read_header().map_err(|e| {
        napi::Error::new(
            napi::Status::GenericFailure,
            format!("读取 VCF header 失败: {}", e),
        )
    })?;

    let use_min_qual = !min_quality.is_nan();
    let use_chrom = !chromosome.is_empty();
    let use_region = region_start > 0 && region_end > 0;
    let use_max = max_variants > 0;

    let mut total: u64 = 0;
    let mut filtered: u64 = 0;
    let mut variants: Vec<VcfVariant> = Vec::new();

    for result in reader.records() {
        let record = result.map_err(|e| {
            napi::Error::new(napi::Status::GenericFailure, format!("解析错误: {}", e))
        })?;
        total += 1;

        // 转为 RecordBuf 以便使用高级访问器
        let rb =
            vcf::variant::RecordBuf::try_from_variant_record(&header, &record).map_err(|e| {
                napi::Error::new(napi::Status::GenericFailure, format!("记录转换失败: {}", e))
            })?;

        let chrom = rb.reference_sequence_name().to_string();

        // 染色体过滤
        if use_chrom && chrom != chromosome {
            continue;
        }

        // 位置过滤（1-based）
        let pos = rb.variant_start().map(|p| p.get() as i64).unwrap_or(0);
        if use_region && (pos < region_start || pos > region_end) {
            continue;
        }

        // QUAL 过滤
        let qual = rb.quality_score().unwrap_or(0.0) as f64;
        if use_min_qual && qual < min_quality {
            continue;
        }

        // PASS 过滤
        if pass_only && !rb.filters().is_pass() {
            continue;
        }

        // 提取字段（reference_bases/alternate_bases 返回 &str；ids/filters 需导入 trait）
        let id = rb
            .ids()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(";");
        let reference = rb.reference_bases().to_string();
        let alt: Vec<String> = rb
            .alternate_bases()
            .iter()
            .map(|result| result.map(|a| a.to_string()).unwrap_or_default())
            .collect();
        let alternate = alt.join(",");
        let filter = if rb.filters().is_pass() {
            "PASS".to_string()
        } else {
            rb.filters()
                .iter(&header)
                .map(|result| result.map(|s| s.to_string()).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(";")
        };

        variants.push(VcfVariant {
            chromosome: chrom,
            position: pos,
            id,
            reference,
            alternate,
            quality: qual,
            filter,
            info: rb
                .info()
                .iter(&header)
                .map(|result| {
                    result
                        .map(|(key, value)| {
                            let k = key.to_string();
                            match value {
                                Some(v) => format!("{}={:?}", k, v),
                                None => k,
                            }
                        })
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(";"),
        });

        filtered += 1;
        if use_max && filtered >= max_variants as u64 {
            break;
        }
    }

    Ok(VcfFilterResult {
        total_count: total as i64,
        filtered_count: filtered as i64,
        variants,
    })
}
