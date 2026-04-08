mod image;
mod pager;

use clap::Parser;
use std::io::{self, IsTerminal, Read};

#[derive(Parser)]
#[command(
    name = "lessi",
    about = "A terminal pager with sixel/kitty graphics support",
    version
)]
struct Args {
    /// File to display. Reads from stdin if not provided.
    file: Option<String>,

    /// Force paging even if content fits in terminal.
    #[arg(short = 'F', long = "force")]
    force: bool,
}

fn main() {
    let args = Args::parse();

    let input = match &args.file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) => {
                eprintln!("lessi: {}: {}", path, err);
                std::process::exit(1);
            }
        },
        None => {
            if io::stdin().is_terminal() {
                eprintln!("lessi: missing filename (\"lessi --help\" for help)");
                std::process::exit(1);
            }
            let mut buf = String::new();
            if let Err(err) = io::stdin().read_to_string(&mut buf) {
                eprintln!("lessi: error reading stdin: {}", err);
                std::process::exit(1);
            }
            buf
        }
    };

    let (cell_w, cell_h) = image::query_cell_size();
    let (cleaned_lines, images) = image::process_input(&input, cell_w, cell_h);
    let parsed_lines = pager::parse_content_to_lines(&cleaned_lines);

    let is_tty = io::stdout().is_terminal();
    let should_page = if !is_tty {
        false
    } else if args.force {
        true
    } else {
        !pager::fits_in_viewport(parsed_lines.len())
    };

    if should_page {
        let filename = args.file.clone();
        if let Err(err) = pager::run_pager(parsed_lines, images, filename, cell_h) {
            eprintln!("lessi: pager error: {}", err);
            std::process::exit(1);
        }
    } else {
        // Output directly without paging
        for line in &cleaned_lines {
            println!("{}", line);
        }
        // Also emit images inline (when outputting to terminal without paging)
        if is_tty {
            for img in &images {
                io::Write::write_all(&mut io::stdout(), &img.data).ok();
            }
        }
    }
}
