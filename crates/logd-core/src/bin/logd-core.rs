//! JSON CLI for agents and scripts operating on large log files.

use anyhow::{anyhow, bail, Context, Result};
use logd_core::autocomplete::FieldCatalog;
use logd_core::index::LineIndex;
use logd_core::matcher::MatcherSet;
use logd_core::progress::Progress;
use logd_core::query::{CompileOptions, Query};
use logd_core::scan::{scan_all, scan_query_all, ScanOutcome};
use logd_core::source::{Encoding, FileSource};
use logd_core::{LogdFile, TatFile};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

const DEFAULT_MAX_LINES: usize = 100;
const DEFAULT_SAMPLE_LINES: usize = 1_000_000;

#[derive(Debug, Default)]
struct Args {
    positional: Vec<String>,
    options: HashMap<String, String>,
    flags: HashSet<String>,
}

#[derive(Serialize)]
struct Info {
    path: String,
    file_name: String,
    bytes: u64,
    encoding: String,
    bom_bytes: usize,
    lines: u64,
    indexed_bytes: u64,
    index_complete: bool,
}
#[derive(Serialize)]
struct LineRecord {
    line: u64,
    text: String,
    truncated: bool,
}
#[derive(Serialize)]
struct MatchRecord {
    line: u64,
    text: String,
    truncated: bool,
}

fn usage() -> &'static str {
    "用法: logd.exe <命令> [选项]\n\n面向大模型/脚本的日志分析 CLI，输出 JSON 或 JSON Lines。\n\n命令:\n  info <日志>                         文件元信息、编码和行数\n  read <日志> [--line N] [--count N]    读取指定行（1-based）\n  query <日志> <表达式>                 按查询语言返回命中行\n  filter <日志> <配置.tat|配置.logd>    按过滤器配置返回命中行\n  suggest <日志> <输入> [--limit N]      字段/值联想\n  config <配置.tat|配置.logd>           以 JSON 输出配置\n\n通用选项:\n  --max-lines N       read/query/filter 最多输出 N 行（默认 100）\n  --max-bytes N       单行最多输出 N 字节（默认 4096）\n  --encoding NAME     覆盖编码，如 utf-8、gb18030\n  --json              对 read 使用 JSON 数组（默认 JSON Lines）\n  -h, --help          显示帮助\n\nquery 表达式示例: 'level>=W and tag:Render'、'\"AEtable\"'、'msg~/timeout/i'"
}

pub fn is_cli_command(command: Option<&str>) -> bool {
    matches!(
        command,
        Some("-h" | "--help" | "info" | "read" | "query" | "filter" | "suggest" | "config")
    )
}

#[allow(dead_code)]
fn main() {
    if let Err(error) = run() {
        eprintln!("错误: {error:#}");
        std::process::exit(1);
    }
}

pub fn run_from<I>(mut it: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    let Some(command) = it.next() else {
        println!("{}", usage());
        return Ok(());
    };
    if command == "-h" || command == "--help" {
        println!("{}", usage());
        return Ok(());
    }
    let args = parse_args(it.collect())?;
    if args.flags.contains("help") {
        println!("{}", usage());
        return Ok(());
    }
    match command.as_str() {
        "info" => cmd_info(&args),
        "read" => cmd_read(&args),
        "query" => cmd_query(&args, false),
        "filter" => cmd_query(&args, true),
        "suggest" => cmd_suggest(&args),
        "config" => cmd_config(&args),
        _ => bail!("未知命令 `{command}`。可用命令: info, read, query, filter, suggest, config"),
    }
}

#[allow(dead_code)]
fn run() -> Result<()> {
    run_from(std::env::args().skip(1))
}

fn parse_args(values: Vec<String>) -> Result<Args> {
    let mut out = Args::default();
    let mut i = 0;
    while i < values.len() {
        let value = &values[i];
        if value == "-h" || value == "--help" {
            out.flags.insert("help".into());
        } else if let Some(name) = value.strip_prefix("--") {
            if name.is_empty() {
                bail!("无效选项");
            }
            if let Some((key, val)) = name.split_once('=') {
                out.options.insert(key.to_owned(), val.to_owned());
            } else if name == "json" {
                out.flags.insert(name.to_owned());
            } else {
                let val = values
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--{name} 缺少参数"))?;
                if val.starts_with('-') {
                    bail!("--{name} 缺少参数");
                }
                out.options.insert(name.to_owned(), val.clone());
                i += 1;
            }
        } else {
            out.positional.push(value.clone());
        }
        i += 1;
    }
    Ok(out)
}

fn required<'a>(args: &'a Args, index: usize, label: &str) -> Result<&'a str> {
    args.positional
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("缺少{label}"))
}
fn option_usize(args: &Args, name: &str, default: usize) -> Result<usize> {
    args.options
        .get(name)
        .map(|v| v.parse().with_context(|| format!("--{name} 必须是正整数")))
        .unwrap_or(Ok(default))
}
fn open_source(args: &Args, path: &str) -> Result<FileSource> {
    let mut source = FileSource::open(path)?;
    if let Some(name) = args.options.get("encoding") {
        source.set_encoding(parse_encoding(name)?);
    }
    Ok(source)
}
fn parse_encoding(name: &str) -> Result<Encoding> {
    let n = name.to_ascii_lowercase().replace(['_', '-', '/', ' '], "");
    Encoding::ALL
        .into_iter()
        .find(|e| {
            e.label()
                .to_ascii_lowercase()
                .replace(['_', '-', '/', ' '], "")
                .starts_with(&n)
        })
        .ok_or_else(|| anyhow!("不支持编码 `{name}`"))
}
fn build_index(source: &FileSource) -> Result<LineIndex> {
    LineIndex::build_full(source.data(), &Progress::new(source.len()))
        .ok_or_else(|| anyhow!("索引被取消"))
}
fn line_spans(source: &FileSource, index: &LineIndex, first: u64, count: usize) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    index.line_spans(source.data(), first, count, &mut out);
    out
}
fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn cmd_info(args: &Args) -> Result<()> {
    let path = required(args, 0, "日志路径")?;
    let source = open_source(args, path)?;
    let index = build_index(&source)?;
    print_json(&Info {
        path: source.path().display().to_string(),
        file_name: source.file_name(),
        bytes: source.len(),
        encoding: source.encoding().label().into(),
        bom_bytes: source.bom_len(),
        lines: index.total_lines,
        indexed_bytes: index.indexed_bytes,
        index_complete: index.complete,
    })
}

fn cmd_read(args: &Args) -> Result<()> {
    let path = required(args, 0, "日志路径")?;
    let source = open_source(args, path)?;
    let index = build_index(&source)?;
    let first = args
        .options
        .get("line")
        .map(|v| v.parse::<u64>().context("--line 必须是正整数"))
        .transpose()?
        .unwrap_or(1)
        .max(1)
        - 1;
    let count = option_usize(
        args,
        "count",
        option_usize(args, "max-lines", DEFAULT_MAX_LINES)?,
    )?;
    let max_bytes = option_usize(args, "max-bytes", 4096)?;
    let records: Vec<_> = line_spans(&source, &index, first, count)
        .into_iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let rendered = logd_core::render::prepare_plain(
                &source.data()[start as usize..end as usize],
                source.encoding(),
                max_bytes,
            );
            LineRecord {
                line: first + i as u64 + 1,
                text: rendered.text,
                truncated: rendered.truncated,
            }
        })
        .collect();
    if args.flags.contains("json") {
        print_json(&records)
    } else {
        for record in records {
            print_json(&record)?;
        }
        Ok(())
    }
}

fn cmd_query(args: &Args, use_filter: bool) -> Result<()> {
    let path = required(args, 0, "日志路径")?;
    let expression = required(
        args,
        1,
        if use_filter {
            "配置路径"
        } else {
            "查询表达式"
        },
    )?;
    let source = open_source(args, path)?;
    let index = build_index(&source)?;
    let outcome = if use_filter {
        let file = if expression.to_ascii_lowercase().ends_with(".tat") {
            TatFile::load(expression)?.into()
        } else {
            LogdFile::load(expression)?
        };
        let matcher = MatcherSet::new(file.filters, source.encoding())?;
        scan_all(
            source.data(),
            &index,
            &matcher,
            &Progress::new(index.indexed_bytes),
        )
        .ok_or_else(|| anyhow!("扫描被取消"))?
    } else {
        let query = Query::parse(
            expression,
            CompileOptions {
                encoding: Some(source.encoding()),
                ..Default::default()
            },
        )?;
        scan_query_all(
            source.data(),
            &index,
            &query,
            &Progress::new(index.indexed_bytes),
        )
        .ok_or_else(|| anyhow!("扫描被取消"))?
    };
    let max_lines = option_usize(args, "max-lines", DEFAULT_MAX_LINES)?;
    let max_bytes = option_usize(args, "max-bytes", 4096)?;
    let lines: Box<dyn Iterator<Item = u64>> = match outcome {
        // Do not materialize every line of a 50GB file for an empty query.
        ScanOutcome::AllVisible => Box::new((0..index.total_lines).take(max_lines)),
        ScanOutcome::Matched(lines) => Box::new(lines.into_iter().take(max_lines)),
    };
    for line in lines {
        let Some((start, end)) = line_spans(&source, &index, line, 1).into_iter().next() else {
            continue;
        };
        let rendered = logd_core::render::prepare_plain(
            &source.data()[start as usize..end as usize],
            source.encoding(),
            max_bytes,
        );
        print_json(&MatchRecord {
            line: line + 1,
            text: rendered.text,
            truncated: rendered.truncated,
        })?;
    }
    Ok(())
}

fn cmd_suggest(args: &Args) -> Result<()> {
    let path = required(args, 0, "日志路径")?;
    let input = required(args, 1, "联想输入")?;
    let source = open_source(args, path)?;
    let limit = option_usize(args, "limit", 20)?;
    let catalog = FieldCatalog::from_data(source.data(), source.encoding(), DEFAULT_SAMPLE_LINES);
    for s in catalog.suggest(input, limit) {
        print_json(
            &serde_json::json!({"field": s.field.map(|f| f.name()), "value": s.value, "expression": s.expression, "score": s.score}),
        )?;
    }
    Ok(())
}
fn cmd_config(args: &Args) -> Result<()> {
    let path = required(args, 0, "配置路径")?;
    if path.to_ascii_lowercase().ends_with(".tat") {
        let file: LogdFile = TatFile::load(path)?.into();
        print_json(&file)
    } else {
        print_json(&LogdFile::load(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_options() {
        let a = parse_args(vec!["--max-lines".into(), "3".into(), "x".into()]).unwrap();
        assert_eq!(a.options["max-lines"], "3");
        assert_eq!(a.positional, vec!["x"]);
    }
}
