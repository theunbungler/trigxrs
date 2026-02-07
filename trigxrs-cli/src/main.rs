//! trigxrs-cli: Command-line tools for trigram-based code search

use std::fs;
use std::io::BufWriter;
use std::path::Path;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "trigxrs")]
#[command(about = "Trigram-based code search", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build an index from a directory
    Index {
        /// Directory to index
        #[arg(required = true)]
        dir: String,

        /// Output index file
        #[arg(short, long, default_value = "index.zrst")]
        output: String,

        /// Maximum shard size in bytes
        #[arg(long, default_value = "104857600")]
        shard_size: usize,

        /// Glob patterns to include
        #[arg(long)]
        include: Vec<String>,

        /// Glob patterns to exclude
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Search an index
    Search {
        /// Index file(s) to search
        #[arg(required = true)]
        index: Vec<String>,

        /// Search pattern
        #[arg(required = true)]
        pattern: String,

        /// File glob pattern
        #[arg(short, long)]
        file: Option<String>,

        /// Number of context lines
        #[arg(short, long, default_value = "0")]
        context: usize,

        /// Case insensitive search
        #[arg(short = 'i', long)]
        case_insensitive: bool,

        /// Treat pattern as regex
        #[arg(short, long)]
        regex: bool,
    },
}

fn cmd_index(
    dir: &str,
    output: &str,
    shard_size: usize,
    include: &[String],
    exclude: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let dir_path = Path::new(dir);
    if !dir_path.is_dir() {
        return Err(format!("{} is not a directory", dir).into());
    }

    // Compile glob patterns
    let include_pats: Vec<glob::Pattern> = include
        .iter()
        .map(|p| glob::Pattern::new(p))
        .collect::<Result<_, _>>()?;
    let exclude_pats: Vec<glob::Pattern> = exclude
        .iter()
        .map(|p| glob::Pattern::new(p))
        .collect::<Result<_, _>>()?;

    let opts = trigxrs_index::BuilderOptions {
        shard_max: shard_size,
        ..Default::default()
    };
    let mut builder = trigxrs_index::IndexBuilder::with_options(opts);

    let mut files_indexed: usize = 0;
    let mut files_skipped: usize = 0;
    let mut shards_written: usize = 0;
    let mut total_bytes: u64 = 0;

    for entry in walkdir::WalkDir::new(dir_path)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden entries (name starts with '.')
            e.file_name()
                .to_str()
                .map(|s| !s.starts_with('.'))
                .unwrap_or(false)
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let full_path = entry.path();
        let rel_path = full_path
            .strip_prefix(dir_path)
            .unwrap_or(full_path)
            .to_string_lossy();

        // Check include patterns (if any specified, file must match at least one)
        if !include_pats.is_empty()
            && !include_pats.iter().any(|p| p.matches(&rel_path))
        {
            continue;
        }

        // Check exclude patterns
        if exclude_pats.iter().any(|p| p.matches(&rel_path)) {
            continue;
        }

        let content = fs::read(full_path)?;

        // Check if we need to flush the current shard
        if builder.file_count() > 0 && builder.would_exceed_shard(content.len()) {
            let shard_path = shard_name(output, shards_written);
            let file = fs::File::create(&shard_path)?;
            let writer = BufWriter::new(file);
            builder.write_shard(writer)?;
            eprintln!("Wrote shard: {}", shard_path);
            shards_written += 1;
        }

        match builder.add_file(&rel_path, &content)? {
            trigxrs_index::SkipReason::None => {
                files_indexed += 1;
                total_bytes += content.len() as u64;
            }
            reason => {
                files_skipped += 1;
                eprintln!("Skipped {}: {:?}", rel_path, reason);
            }
        }
    }

    // Write final shard
    if builder.file_count() > 0 {
        let shard_path = if shards_written == 0 {
            output.to_string()
        } else {
            shard_name(output, shards_written)
        };
        let file = fs::File::create(&shard_path)?;
        let writer = BufWriter::new(file);
        builder.write_shard(writer)?;
        shards_written += 1;

        // If we wrote multiple shards, rename the first one
        // (first shard was written as output.00000, but if only one total, use output as-is)
    }

    // If we wrote multiple shards but the very first was written with shard_name,
    // that's already correct. But if shards_written ended up being 1 and we used
    // output directly, that's also correct. Handle the edge case where the first
    // shard was written with shard_name (shards > 0 when final shard is written).
    // Actually, the logic above already handles this: intermediate shards always use
    // shard_name, and the final shard uses output directly only if it's the only shard.

    eprintln!(
        "Indexed {} files ({} bytes), skipped {}, {} shard(s) written",
        files_indexed, total_bytes, files_skipped, shards_written
    );

    Ok(())
}

/// Generate a shard filename: output.00000, output.00001, etc.
fn shard_name(base: &str, index: usize) -> String {
    format!("{}.{:05}", base, index)
}

fn cmd_search(
    index_paths: &[String],
    pattern: &str,
    file_filter: Option<&str>,
    context: usize,
    case_insensitive: bool,
    is_regex: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_pat = file_filter
        .map(|p| glob::Pattern::new(p))
        .transpose()?;

    let query = trigxrs_search::Query {
        pattern: pattern.to_string(),
        is_regex,
        options: trigxrs_search::SearchOptions {
            case_sensitive: !case_insensitive,
            max_matches: 0,
            context_lines: context,
            file_pattern: file_pat,
        },
    };

    let searcher = trigxrs_search::Searcher::open(index_paths)?;
    let result = searcher.search(&query)?;

    if context > 0 && !result.contexts.is_empty() {
        let mut last_file_idx: Option<usize> = None;
        let mut last_printed_line: Option<usize> = None;

        for ctx in &result.contexts {
            let file_entry = searcher.file_entry(ctx.m.file_idx).unwrap();
            let match_line_0 = ctx.m.line_number.unwrap_or(1).saturating_sub(1) as usize;

            let need_separator = if ctx.m.file_idx != last_file_idx.unwrap_or(usize::MAX) {
                last_printed_line = None;
                last_file_idx = Some(ctx.m.file_idx);
                false
            } else {
                let ctx_start = match_line_0.saturating_sub(context);
                match last_printed_line {
                    Some(last) if ctx_start > last + 1 => true,
                    _ => false,
                }
            };

            if need_separator {
                println!("--");
            }

            let before_start_line = match_line_0 - ctx.before.len();

            for (i, line) in ctx.before.iter().enumerate() {
                let line_idx = before_start_line + i;
                if let Some(last) = last_printed_line {
                    if line_idx <= last {
                        continue;
                    }
                }
                let line_num = line_idx + 1;
                println!("{}-{}-{}", file_entry.path, line_num, line);
                last_printed_line = Some(line_idx);
            }

            if last_printed_line.is_none_or(|last| match_line_0 > last) {
                let line_num = match_line_0 + 1;
                println!("{}:{}:{}", file_entry.path, line_num, ctx.line_text);
                last_printed_line = Some(match_line_0);
            }

            for (i, line) in ctx.after.iter().enumerate() {
                let line_idx = match_line_0 + 1 + i;
                if let Some(last) = last_printed_line {
                    if line_idx <= last {
                        continue;
                    }
                }
                let line_num = line_idx + 1;
                println!("{}-{}-{}", file_entry.path, line_num, line);
                last_printed_line = Some(line_idx);
            }
        }
    } else {
        for m in &result.matches {
            let file_entry = searcher.file_entry(m.file_idx).unwrap();
            let line_num = m.line_number.unwrap_or(0);

            if let Some(content) = searcher.file_content(m.file_idx) {
                if let Ok(text) = std::str::from_utf8(content) {
                    let lines: Vec<&str> = text.lines().collect();
                    let line_idx = line_num.saturating_sub(1) as usize;
                    if line_idx < lines.len() {
                        println!("{}:{}:{}", file_entry.path, line_num, lines[line_idx]);
                        continue;
                    }
                }
            }
            println!("{}:{}:<binary>", file_entry.path, line_num);
        }
    }

    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Index {
            dir,
            output,
            shard_size,
            include,
            exclude,
        } => cmd_index(&dir, &output, shard_size, &include, &exclude),
        Commands::Search {
            index,
            pattern,
            file,
            context,
            case_insensitive,
            regex,
        } => cmd_search(
            &index,
            &pattern,
            file.as_deref(),
            context,
            case_insensitive,
            regex,
        ),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
