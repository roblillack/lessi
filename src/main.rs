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
        // Output directly without paging.
        // Emit images inline at their correct line positions so the
        // terminal renders them where the spacer lines reserve space.
        let mut img_idx = 0;
        let mut stdout = io::stdout();
        for (line_idx, line) in cleaned_lines.iter().enumerate() {
            println!("{}", line);
            // Emit any images that start on this line
            while img_idx < images.len() && images[img_idx].line_idx == line_idx {
                if is_tty {
                    io::Write::write_all(&mut stdout, &images[img_idx].data).ok();
                }
                img_idx += 1;
            }
        }
    }
}
