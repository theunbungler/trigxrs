//! trigxrs-cli: Command-line tools for trigram-based code search

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
        #[arg(short, long, default_value = "2")]
        context: usize,

        /// Case insensitive search
        #[arg(short = 'i', long)]
        case_insensitive: bool,

        /// Treat pattern as regex
        #[arg(short, long)]
        regex: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Index {
            dir,
            output,
            shard_size,
            include,
            exclude,
        } => {
            println!("Indexing {} -> {} (shard_size={})", dir, output, shard_size);
            if !include.is_empty() {
                println!("Include: {:?}", include);
            }
            if !exclude.is_empty() {
                println!("Exclude: {:?}", exclude);
            }
            // TODO: Implement indexing
            eprintln!("Index command not yet implemented");
        }
        Commands::Search {
            index,
            pattern,
            file,
            context,
            case_insensitive,
            regex,
        } => {
            println!(
                "Searching {:?} for '{}' (context={}, case_insensitive={}, regex={})",
                index, pattern, context, case_insensitive, regex
            );
            if let Some(f) = file {
                println!("File filter: {}", f);
            }
            // TODO: Implement search
            eprintln!("Search command not yet implemented");
        }
    }
}
