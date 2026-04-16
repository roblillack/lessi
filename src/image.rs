//! Image protocol detection, parsing, and storage for sixel and kitty graphics.

/// An inline image extracted from the input stream.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct InlineImage {
    /// The line index in the final (expanded) line model where the image starts.
    pub line_idx: usize,
    /// The column (character position) where the image was found on that line.
    pub col: usize,
    /// How many terminal rows this image occupies.
    pub height_rows: usize,
    /// How many terminal columns this image occupies.
    pub width_cols: usize,
    /// The raw escape sequence data (complete, ready to emit).
    pub data: Vec<u8>,
    /// The protocol used.
    pub protocol: ImageProtocol,
    /// Number of sixel data rows in the image (only meaningful for Sixel).
    /// A sixel row is 6 pixels tall.
    pub sixel_row_count: usize,
    /// Byte offset within `data` where the sixel header ends and repeatable
    /// row data begins (right after color definitions, at the first data character).
    /// For kitty images this is 0.
    pub sixel_data_start: usize,
    /// Byte offsets within `data` of each sixel row separator ('-').
    /// Entry i gives the byte index of the '-' that ends row i.
    /// For kitty images this is empty.
    pub sixel_row_offsets: Vec<usize>,
    /// All sixel color definitions found anywhere in the data, as raw byte sequences.
    /// Each entry is a complete definition like b"#0;2;100;0;0" (without trailing data).
    /// Needed to reconstruct the palette when clipping from the top.
    pub sixel_color_defs: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Sixel,
    Kitty,
}

/// Result of scanning a single line for image sequences.
struct LineScanResult {
    /// The line with image sequences replaced by placeholder spaces.
    cleaned: String,
    /// Images found on this line.
    images: Vec<ExtractedImage>,
}

/// An image extracted during line scanning (before line index is assigned).
struct ExtractedImage {
    col: usize,
    width_cols: usize,
    height_rows: usize,
    data: Vec<u8>,
    protocol: ImageProtocol,
    sixel_row_count: usize,
    sixel_data_start: usize,
    sixel_row_offsets: Vec<usize>,
    sixel_color_defs: Vec<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Terminal cell size detection
// ---------------------------------------------------------------------------

/// Query the terminal for cell size in pixels.
/// Tries, in order:
///   1. TIOCGWINSZ ioctl (pixel fields, often zero on many terminals)
///   2. CSI 16 t  -- xterm "Report Cell Size" (widely supported by sixel-capable terminals)
///
/// Returns (cell_width, cell_height) in pixels, or a conservative default.
pub fn query_cell_size() -> (usize, usize) {
    #[cfg(unix)]
    {
        // --- Approach 1: TIOCGWINSZ ---
        if let Some(size) = query_cell_size_ioctl() {
            return size;
        }
        // --- Approach 2: CSI 16 t escape sequence ---
        if let Some(size) = query_cell_size_escape() {
            return size;
        }
    }
    // Conservative fallback
    (8, 16)
}

#[cfg(unix)]
fn query_cell_size_ioctl() -> Option<(usize, usize)> {
    use std::mem::MaybeUninit;
    for fd in [1i32, 2, 0] {
        let mut ws = MaybeUninit::<[u16; 4]>::uninit();
        let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, ws.as_mut_ptr()) };
        if ret == 0 {
            let ws = unsafe { ws.assume_init() };
            let rows = ws[0] as usize;
            let cols = ws[1] as usize;
            let xpix = ws[2] as usize;
            let ypix = ws[3] as usize;
            if xpix > 0 && ypix > 0 && rows > 0 && cols > 0 {
                return Some((xpix / cols, ypix / rows));
            }
        }
    }
    None
}

/// Query cell size via the CSI 16 t escape sequence.
/// Sends the query to the terminal and reads the response.
/// Response format: ESC [ 6 ; cell_h ; cell_w t
#[cfg(unix)]
fn query_cell_size_escape() -> Option<(usize, usize)> {
    use std::io::{Read, Write};

    // We need a tty fd for both writing the query and reading the response.
    // Open /dev/tty directly to avoid issues with redirected stdin/stdout.
    let mut tty = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(f) => f,
        Err(_) => return None,
    };

    let tty_fd = {
        use std::os::unix::io::AsRawFd;
        tty.as_raw_fd()
    };

    // Save terminal state and switch to raw mode
    let mut old_termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(tty_fd, old_termios.as_mut_ptr()) } != 0 {
        return None;
    }
    let old_termios = unsafe { old_termios.assume_init() };
    let mut raw = old_termios;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 1; // 100ms timeout
    if unsafe { libc::tcsetattr(tty_fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    // Flush any pending input
    let _ = unsafe { libc::tcflush(tty_fd, libc::TCIFLUSH) };

    // Send CSI 16 t
    let wrote = tty.write(b"\x1b[16t").ok();
    let _ = tty.flush();

    let result = if wrote.is_some() {
        // Read response: ESC [ 6 ; Ps1 ; Ps2 t
        let mut buf = [0u8; 64];
        let mut total = 0usize;
        // Read with timeout (VTIME handles this)
        for _ in 0..10 {
            match tty.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    // Check if we got the 't' terminator
                    if buf[..total].contains(&b't') {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        parse_cell_size_response(&buf[..total])
    } else {
        None
    };

    // Restore terminal state
    unsafe { libc::tcsetattr(tty_fd, libc::TCSANOW, &old_termios) };

    result
}

/// Parse a CSI 16 t response: ESC [ 6 ; cell_h ; cell_w t
#[cfg(unix)]
fn parse_cell_size_response(buf: &[u8]) -> Option<(usize, usize)> {
    // Find the sequence starting with ESC [
    let esc_pos = buf.iter().position(|&b| b == 0x1b)?;
    if esc_pos + 1 >= buf.len() || buf[esc_pos + 1] != b'[' {
        return None;
    }
    let after_csi = &buf[esc_pos + 2..];
    // Find the 't' terminator
    let t_pos = after_csi.iter().position(|&b| b == b't')?;
    let params_str = std::str::from_utf8(&after_csi[..t_pos]).ok()?;
    let parts: Vec<&str> = params_str.split(';').collect();
    // Expected: "6;cell_h;cell_w"
    if parts.len() >= 3 && parts[0] == "6" {
        let cell_h = parts[1].parse::<usize>().ok()?;
        let cell_w = parts[2].parse::<usize>().ok()?;
        if cell_h > 0 && cell_w > 0 {
            return Some((cell_w, cell_h));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Line scanning
// ---------------------------------------------------------------------------

/// Scan a line for inline image sequences (sixel and kitty),
/// replacing them with placeholder spaces and extracting the image data.
fn scan_line_for_images(line: &str, cell_w: usize, cell_h: usize) -> LineScanResult {
    let bytes = line.as_bytes();
    let mut cleaned = String::with_capacity(line.len());
    let mut images = Vec::new();
    let mut i = 0;
    let mut col = 0usize;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            // Check for sixel: ESC P ... ESC backslash
            if bytes[i + 1] == b'P' {
                if let Some((end, data)) = find_sixel_end(bytes, i) {
                    let info = analyze_sixel(&data, cell_w, cell_h);
                    let placeholder_width = info.width_cols.max(1);
                    for _ in 0..placeholder_width {
                        cleaned.push(' ');
                    }
                    images.push(ExtractedImage {
                        col,
                        width_cols: placeholder_width,
                        height_rows: info.height_rows.max(1),
                        data,
                        protocol: ImageProtocol::Sixel,
                        sixel_row_count: info.sixel_row_count,
                        sixel_data_start: info.sixel_data_start,
                        sixel_row_offsets: info.sixel_row_offsets,
                        sixel_color_defs: info.sixel_color_defs,
                    });
                    col += placeholder_width;
                    i = end;
                    continue;
                }
            }
            // Check for kitty: ESC _ G ... ESC backslash
            if bytes[i + 1] == b'_' {
                if let Some((end, data)) = find_kitty_end(bytes, i) {
                    let (w, h) = parse_kitty_dimensions(&data, cell_w, cell_h);
                    let placeholder_width = w.max(1);
                    for _ in 0..placeholder_width {
                        cleaned.push(' ');
                    }
                    images.push(ExtractedImage {
                        col,
                        width_cols: placeholder_width,
                        height_rows: h.max(1),
                        data,
                        protocol: ImageProtocol::Kitty,
                        sixel_row_count: 0,
                        sixel_data_start: 0,
                        sixel_row_offsets: Vec::new(),
                        sixel_color_defs: Vec::new(),
                    });
                    col += placeholder_width;
                    i = end;
                    continue;
                }
            }

            // Not a recognised image sequence — copy to `cleaned` so
            // downstream ANSI parsing still sees it, but do NOT advance
            // `col` (escape sequences occupy no screen columns).
            if bytes[i + 1] == b'[' {
                // CSI: ESC [ <params> <final byte 0x40..0x7E>
                cleaned.push('\x1b');
                cleaned.push('[');
                let mut j = i + 2;
                while j < bytes.len() {
                    cleaned.push(bytes[j] as char);
                    let done = (0x40..=0x7e).contains(&bytes[j]);
                    j += 1;
                    if done {
                        break;
                    }
                }
                i = j;
                continue;
            }
            if bytes[i + 1] == b']' {
                // OSC: ESC ] ... (BEL | ESC \)
                cleaned.push('\x1b');
                cleaned.push(']');
                let mut j = i + 2;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        cleaned.push(bytes[j] as char);
                        j += 1;
                        break;
                    }
                    if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                        cleaned.push('\x1b');
                        cleaned.push('\\');
                        j += 2;
                        break;
                    }
                    cleaned.push(bytes[j] as char);
                    j += 1;
                }
                i = j;
                continue;
            }
            // Other two-byte ESC sequence (e.g. ESC c, ESC 7, …)
            cleaned.push('\x1b');
            cleaned.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }

        // Regular character: copy through
        let ch = line[i..].chars().next().unwrap_or(' ');
        let len = ch.len_utf8();
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        cleaned.push(ch);
        col += w;
        i += len;
    }

    LineScanResult { cleaned, images }
}

// ---------------------------------------------------------------------------
// Sixel analysis
// ---------------------------------------------------------------------------

struct SixelInfo {
    width_cols: usize,
    height_rows: usize,
    sixel_row_count: usize,
    /// Byte offset in the data where the first sixel data row starts
    /// (after header + color definitions).
    sixel_data_start: usize,
    /// Byte offset of each '-' (Graphics New Line) separator.
    sixel_row_offsets: Vec<usize>,
    /// All color definitions found in the entire sixel stream.
    sixel_color_defs: Vec<Vec<u8>>,
}

/// Analyze a complete sixel sequence to extract dimensions and row offsets.
fn analyze_sixel(data: &[u8], cell_w: usize, cell_h: usize) -> SixelInfo {
    let s = String::from_utf8_lossy(data);

    // Try raster attributes first: "Pan;Pad;Ph;Pv after 'q'
    let mut pixel_w = 0usize;
    let mut pixel_h = 0usize;

    let q_byte_pos = data.iter().position(|&b| b == b'q');

    if let Some(q_pos) = s.find('q') {
        let after_q = &s[q_pos + 1..];
        if let Some(stripped) = after_q.strip_prefix('"') {
            // Parse raster attributes
            let raster_end = stripped.find(|c: char| {
                c == '#' || c == '!' || c == '$' || c == '-' || ('?'..='~').contains(&c)
            });
            if let Some(end) = raster_end {
                let raster = &stripped[..end];
                let parts: Vec<&str> = raster.split(';').collect();
                if parts.len() >= 4 {
                    pixel_w = parts[2].parse().unwrap_or(0);
                    pixel_h = parts[3].parse().unwrap_or(0);
                }
            }
        }
    }

    // Scan the data to find row offsets and count rows.
    // We need to find where the actual sixel data starts (after color defs)
    // and record positions of '-' separators.
    let mut row_offsets = Vec::new();
    let mut color_defs: Vec<Vec<u8>> = Vec::new();
    let sixel_data_start = q_byte_pos.map(|p| p + 1).unwrap_or(0);
    let mut rows = 1usize;
    let mut max_width_pixels = 0usize;
    let mut current_width = 0usize;

    let mut i = sixel_data_start;

    // Skip raster attributes if present
    if i < data.len() && data[i] == b'"' {
        while i < data.len()
            && data[i] != b'#'
            && !(data[i] >= b'?' && data[i] <= b'~')
            && data[i] != b'!'
            && data[i] != b'-'
            && data[i] != b'$'
        {
            i += 1;
        }
    }

    // Now scan through the sixel data
    let data_region_start = i;
    while i < data.len() {
        match data[i] {
            b'\x1b' => break, // ST coming
            b'#' => {
                // Color introducer: either a definition (#Pc;Pu;Px;Py;Pz)
                // or just a selection (#Pc). We distinguish by counting semicolons.
                let hash_pos = i;
                i += 1;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    i += 1;
                }
                // Check if this was a definition (contains semicolons after #)
                let fragment = &data[hash_pos..i];
                if fragment.contains(&b';') {
                    color_defs.push(fragment.to_vec());
                }
                continue;
            }
            b'-' => {
                // Graphics New Line
                if current_width > max_width_pixels {
                    max_width_pixels = current_width;
                }
                current_width = 0;
                row_offsets.push(i);
                rows += 1;
            }
            b'$' => {
                // Graphics Carriage Return
                if current_width > max_width_pixels {
                    max_width_pixels = current_width;
                }
                current_width = 0;
            }
            b'!' => {
                // Repeat introducer: !<count><sixel_char>
                i += 1;
                let mut count = 0usize;
                while i < data.len() && data[i].is_ascii_digit() {
                    count = count * 10 + (data[i] - b'0') as usize;
                    i += 1;
                }
                if i < data.len() && data[i] >= b'?' && data[i] <= b'~' {
                    current_width += count.max(1);
                }
                // i now points at the sixel char, will be incremented below
            }
            b'?'..=b'~' => {
                current_width += 1;
            }
            _ => {}
        }
        i += 1;
    }

    if current_width > max_width_pixels {
        max_width_pixels = current_width;
    }

    // If we didn't get pixel dimensions from raster attributes, estimate them
    if pixel_w == 0 {
        pixel_w = max_width_pixels;
    }
    if pixel_h == 0 {
        pixel_h = rows * 6;
    }

    let width_cols = pixel_w.div_ceil(cell_w);
    let height_rows = pixel_h.div_ceil(cell_h);

    SixelInfo {
        width_cols: width_cols.max(1),
        height_rows: height_rows.max(1),
        sixel_row_count: rows,
        sixel_data_start: data_region_start,
        sixel_row_offsets: row_offsets,
        sixel_color_defs: color_defs,
    }
}

// ---------------------------------------------------------------------------
// Sixel clipping
// ---------------------------------------------------------------------------

/// Build a clipped sixel sequence that keeps only the sixel rows corresponding
/// to terminal rows `skip_top .. (height_rows - skip_bottom)`.
///
/// `skip_top`  — number of terminal rows to remove from the top  (0 = keep top)
/// `keep_rows` — maximum number of terminal rows to keep         (0 = nothing)
///
/// Returns `None` if nothing remains after clipping.
pub fn clip_sixel(
    img: &InlineImage,
    skip_top: usize,
    keep_rows: usize,
    cell_h: usize,
) -> Option<Vec<u8>> {
    if keep_rows == 0 || skip_top >= img.height_rows {
        return None;
    }

    let visible_rows = keep_rows.min(img.height_rows - skip_top);

    // Fast path: no clipping needed at all
    if skip_top == 0 && visible_rows >= img.height_rows {
        return Some(img.data.clone());
    }

    // --- Convert terminal rows → sixel rows --------------------------------
    // Each terminal row = cell_h pixels, each sixel row = 6 pixels.
    //
    // Use floor division for keep_sixel_rows so the emitted data never
    // exceeds the visible area.  This prevents sixel pixels from bleeding
    // past the viewport into the status bar — important because not all
    // terminals clip sixel output to the declared raster height.
    let skip_sixel_top = (skip_top * cell_h) / 6;
    let keep_pixels = visible_rows * cell_h;
    let keep_sixel_rows = (keep_pixels / 6).max(1);

    if skip_sixel_top >= img.sixel_row_count {
        return None;
    }

    let end_sixel_row = (skip_sixel_top + keep_sixel_rows).min(img.sixel_row_count);
    if end_sixel_row <= skip_sixel_top {
        return None;
    }

    // --- Determine byte range of the rows we want to keep ------------------
    // Start: right after the '-' that ends (skip_sixel_top - 1), or data_start.
    let data_start = if skip_sixel_top == 0 {
        img.sixel_data_start
    } else if skip_sixel_top <= img.sixel_row_offsets.len() {
        img.sixel_row_offsets[skip_sixel_top - 1] + 1
    } else {
        return None;
    };

    // End: at the '-' that ends (end_sixel_row - 1), or before ST.
    let st_start = find_st_position(&img.data)?;
    let data_end = if end_sixel_row >= img.sixel_row_count {
        // Keep through the last row (up to ST)
        st_start
    } else if end_sixel_row <= img.sixel_row_offsets.len() {
        // Include up to (but not including) the '-' that ends row end_sixel_row-1,
        // but we actually want to include that row's data.  The offset points at the
        // '-' itself; we want everything up to and including the data before it.
        img.sixel_row_offsets[end_sixel_row - 1]
    } else {
        st_start
    };

    if data_start >= data_end {
        return None;
    }

    let kept_data = &img.data[data_start..data_end];

    // --- Rebuild the sequence -----------------------------------------------
    let header = &img.data[..img.sixel_data_start];
    let new_pixel_h = visible_rows * cell_h;
    let adjusted_header = adjust_sixel_raster_height(header, new_pixel_h);

    let color_defs_size: usize = img.sixel_color_defs.iter().map(|d| d.len()).sum();
    let mut result =
        Vec::with_capacity(adjusted_header.len() + color_defs_size + kept_data.len() + 2);
    result.extend_from_slice(&adjusted_header);

    // Always prepend the full palette so colour references resolve correctly
    // even when the defining rows have been clipped away.
    for def in &img.sixel_color_defs {
        result.extend_from_slice(def);
    }

    result.extend_from_slice(kept_data);
    // Append ST (ESC \)
    result.push(0x1b);
    result.push(b'\\');

    Some(result)
}

// ---------------------------------------------------------------------------
// Kitty clipping
// ---------------------------------------------------------------------------

use base64::Engine;

/// Maximum base64 payload bytes per kitty APC chunk.
const KITTY_CHUNK_SIZE: usize = 4096;

/// A parsed chunk from a kitty graphics sequence.
struct KittyChunk {
    /// The control parameter string (between 'G' and ';' or ST).
    control: String,
    /// The payload bytes (between ';' and ST). Empty if no payload.
    payload: Vec<u8>,
}

/// Parsed control parameters from a kitty graphics chunk.
struct KittyControl {
    format: u32,      // f= (24=RGB, 32=RGBA, 100=PNG)
    pixel_w: usize,   // s=
    pixel_h: usize,   // v=
    compressed: bool, // o=z
    /// All other key=value pairs preserved verbatim (a, c, i, p, t, …).
    other_params: Vec<(String, String)>,
}

/// Parse all kitty APC chunks from raw image data.
/// Each chunk has the form: ESC _ G <control> [; <payload>] ESC \
fn parse_kitty_chunks(data: &[u8]) -> Vec<KittyChunk> {
    let mut chunks = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'_' && data[i + 2] == b'G' {
            i += 3; // skip ESC _ G
            let ctrl_start = i;
            while i < data.len() && data[i] != b';' && data[i] != 0x1b {
                i += 1;
            }
            let control = String::from_utf8_lossy(&data[ctrl_start..i]).to_string();

            let mut payload = Vec::new();
            if i < data.len() && data[i] == b';' {
                i += 1;
                let payload_start = i;
                while i < data.len() {
                    if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                        break;
                    }
                    i += 1;
                }
                payload = data[payload_start..i].to_vec();
            }

            if i + 1 < data.len() && data[i] == 0x1b && data[i + 1] == b'\\' {
                i += 2;
            }

            chunks.push(KittyChunk { control, payload });
        } else {
            i += 1;
        }
    }
    chunks
}

/// Extract structured control parameters from the first chunk's control string.
fn parse_kitty_control(control: &str) -> KittyControl {
    let mut format = 32u32;
    let mut pixel_w = 0usize;
    let mut pixel_h = 0usize;
    let mut compressed = false;
    let mut other_params = Vec::new();

    for kv in control.split(',') {
        if let Some((key, value)) = kv.split_once('=') {
            match key {
                "f" => format = value.parse().unwrap_or(32),
                "s" => pixel_w = value.parse().unwrap_or(0),
                "v" => pixel_h = value.parse().unwrap_or(0),
                "o" => compressed = value == "z",
                // Managed by clipper — drop these
                "r" | "m" | "y" | "h" => {}
                _ => other_params.push((key.to_string(), value.to_string())),
            }
        }
    }

    KittyControl {
        format,
        pixel_w,
        pixel_h,
        compressed,
        other_params,
    }
}

/// Rebuild a kitty control string with updated dimensions and format.
///
/// Does NOT emit `r=` (display rows) — the terminal auto-calculates the
/// correct row count from the pixel data.  Emitting an explicit `r=` would
/// cause the terminal to *scale* the image to fit that many rows, which
/// produces visible resizing artefacts when the cropped pixel height doesn't
/// align exactly to cell boundaries.
fn rebuild_kitty_control(
    ctrl: &KittyControl,
    new_format: u32,
    new_pixel_w: usize,
    new_pixel_h: usize,
    new_compressed: bool,
) -> String {
    let mut parts = Vec::new();

    for (k, v) in &ctrl.other_params {
        parts.push(format!("{k}={v}"));
    }

    parts.push(format!("f={new_format}"));
    parts.push(format!("s={new_pixel_w}"));
    parts.push(format!("v={new_pixel_h}"));
    if new_compressed {
        parts.push("o=z".to_string());
    }

    parts.join(",")
}

/// Build a complete (possibly multi-chunk) kitty APC sequence.
fn build_kitty_sequence(control: &str, b64_payload: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();

    if b64_payload.len() <= KITTY_CHUNK_SIZE {
        // Single chunk — no m= needed
        result.extend_from_slice(b"\x1b_G");
        result.extend_from_slice(control.as_bytes());
        result.push(b';');
        result.extend_from_slice(b64_payload);
        result.extend_from_slice(b"\x1b\\");
    } else {
        let total_chunks = b64_payload.len().div_ceil(KITTY_CHUNK_SIZE);
        for (i, chunk) in b64_payload.chunks(KITTY_CHUNK_SIZE).enumerate() {
            result.extend_from_slice(b"\x1b_G");
            if i == 0 {
                result.extend_from_slice(control.as_bytes());
                result.extend_from_slice(b",m=1");
            } else if i < total_chunks - 1 {
                result.extend_from_slice(b"m=1");
            } else {
                result.extend_from_slice(b"m=0");
            }
            result.push(b';');
            result.extend_from_slice(chunk);
            result.extend_from_slice(b"\x1b\\");
        }
    }

    result
}

/// Decode a PNG byte stream into raw pixels, returning (pixels, width, height,
/// color_type, bit_depth).
fn decode_png(data: &[u8]) -> Option<(Vec<u8>, usize, usize, png::ColorType, png::BitDepth)> {
    let decoder = png::Decoder::new(data);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    Some((
        buf,
        frame.width as usize,
        frame.height as usize,
        frame.color_type,
        frame.bit_depth,
    ))
}

/// Encode raw pixels back into a PNG byte stream.
fn encode_png(
    pixels: &[u8],
    width: usize,
    height: usize,
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width as u32, height as u32);
        enc.set_color(color_type);
        enc.set_depth(bit_depth);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(pixels).ok()?;
    }
    Some(buf)
}

/// Build a clipped kitty image by decoding the pixel payload, cropping rows,
/// and re-encoding.  This is analogous to `clip_sixel` — the output contains
/// only the pixels that should be visible, so byte count scales with the
/// visible area rather than the full image.
///
/// `skip_top`  — number of terminal rows to remove from the top  (0 = keep top)
/// `keep_rows` — maximum number of terminal rows to keep         (0 = nothing)
///
/// Returns `None` if nothing remains after clipping.
pub fn clip_kitty(
    img: &InlineImage,
    skip_top: usize,
    keep_rows: usize,
    cell_h: usize,
) -> Option<Vec<u8>> {
    if keep_rows == 0 || skip_top >= img.height_rows {
        return None;
    }

    let visible_rows = keep_rows.min(img.height_rows - skip_top);

    // Fast path: no clipping needed
    if skip_top == 0 && visible_rows >= img.height_rows {
        return Some(img.data.clone());
    }

    let chunks = parse_kitty_chunks(&img.data);
    if chunks.is_empty() {
        return None;
    }

    // --- Decode payload ----------------------------------------------------
    let ctrl = parse_kitty_control(&chunks[0].control);

    // Concatenate base64 payloads from all chunks and decode
    let b64_cat: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.payload.iter().copied())
        .collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&b64_cat)
        .ok()?;

    // Decompress if zlib-compressed (only for raw pixel formats)
    let raw_data = if ctrl.compressed {
        use flate2::read::ZlibDecoder;
        use std::io::Read;
        let mut dec = ZlibDecoder::new(decoded.as_slice());
        let mut out = Vec::new();
        dec.read_to_end(&mut out).ok()?;
        out
    } else {
        decoded
    };

    // --- Decode pixels & determine geometry --------------------------------
    let (pixels, width, height, out_format, out_color, out_depth) = if ctrl.format == 100 {
        // PNG: decode to raw pixels
        let (px, w, h, ct, bd) = decode_png(&raw_data)?;
        (px, w, h, 100u32, ct, bd)
    } else {
        let bpp: usize = if ctrl.format == 24 { 3 } else { 4 };
        let w = ctrl.pixel_w;
        let h = ctrl.pixel_h;
        if w == 0 || h == 0 {
            return None;
        }
        let ct = if bpp == 3 {
            png::ColorType::Rgb
        } else {
            png::ColorType::Rgba
        };
        (raw_data, w, h, ctrl.format, ct, png::BitDepth::Eight)
    };

    if width == 0 || height == 0 {
        return None;
    }

    let bpp = out_color.samples() * (out_depth as usize / 8).max(1);
    let stride = width * bpp;

    // --- Crop pixel rows ---------------------------------------------------
    let skip_pixel_rows = (skip_top * cell_h).min(height);
    let keep_pixel_rows = (visible_rows * cell_h).min(height - skip_pixel_rows);
    if keep_pixel_rows == 0 {
        return None;
    }

    let start = skip_pixel_rows * stride;
    let end = start + keep_pixel_rows * stride;
    if end > pixels.len() {
        return None;
    }
    let cropped = &pixels[start..end];

    // --- Re-encode ---------------------------------------------------------
    let (encoded_payload, final_format, final_compressed) = if out_format == 100 {
        // Re-encode as PNG — compact and self-describing
        let png_bytes = encode_png(cropped, width, keep_pixel_rows, out_color, out_depth)?;
        (png_bytes, 100u32, false)
    } else if ctrl.compressed {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(cropped).ok()?;
        let compressed = enc.finish().ok()?;
        (compressed, ctrl.format, true)
    } else {
        (cropped.to_vec(), ctrl.format, false)
    };

    let b64 = base64::engine::general_purpose::STANDARD.encode(&encoded_payload);

    // --- Build output sequence ---------------------------------------------
    let new_control = rebuild_kitty_control(
        &ctrl,
        final_format,
        width,
        keep_pixel_rows,
        final_compressed,
    );

    Some(build_kitty_sequence(&new_control, b64.as_bytes()))
}

/// Find the byte position where the ST (ESC \) starts in sixel data.
fn find_st_position(data: &[u8]) -> Option<usize> {
    let len = data.len();
    if len >= 2 && data[len - 2] == 0x1b && data[len - 1] == b'\\' {
        Some(len - 2)
    } else if len >= 1 && data[len - 1] == 0x9c {
        Some(len - 1)
    } else {
        // Search backwards
        for i in (0..len.saturating_sub(1)).rev() {
            if data[i] == 0x1b && i + 1 < len && data[i + 1] == b'\\' {
                return Some(i);
            }
        }
        None
    }
}

/// Adjust the raster attributes in a sixel header to reflect a new pixel height.
/// The header is everything from ESC P ... up to (but not including) actual sixel data.
fn adjust_sixel_raster_height(header: &[u8], new_pixel_h: usize) -> Vec<u8> {
    // Look for the raster attributes pattern: "Pan;Pad;Ph;Pv
    let s = String::from_utf8_lossy(header);
    if let Some(q_pos) = s.find('q') {
        let after_q = &s[q_pos + 1..];
        if let Some(stripped) = after_q.strip_prefix('"') {
            // Find the end of raster attributes — they may extend to the
            // end of the header when it was sliced right before the first
            // color definition or data byte.
            let raster_end = stripped
                .find(|c: char| {
                    c == '#' || c == '!' || c == '$' || c == '-' || ('?'..='~').contains(&c)
                })
                .unwrap_or(stripped.len());
            let raster = &stripped[..raster_end];
            let parts: Vec<&str> = raster.split(';').collect();
            if parts.len() >= 4 {
                // Rebuild with adjusted Pv
                let new_raster = format!("{};{};{};{}", parts[0], parts[1], parts[2], new_pixel_h);
                let prefix = &s[..q_pos + 1]; // up to and including 'q'
                let suffix = &stripped[raster_end..]; // after raster attrs
                let result = format!("{}\"{}{}", prefix, new_raster, suffix);
                return result.into_bytes();
            }
        }
    }
    header.to_vec()
}

// ---------------------------------------------------------------------------
// Sequence finders
// ---------------------------------------------------------------------------

/// Find the end of a sixel sequence starting at `start`.
fn find_sixel_end(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    let mut i = start + 2; // skip ESC P
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            let end = i + 2;
            return Some((end, bytes[start..end].to_vec()));
        }
        if bytes[i] == 0x9c {
            let end = i + 1;
            return Some((end, bytes[start..end].to_vec()));
        }
        i += 1;
    }
    None
}

/// Find the end of a kitty graphics sequence starting at `start`.
fn find_kitty_end(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    let mut i = start + 2;
    let mut data = Vec::new();
    loop {
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            let end = i + 2;
            data.extend_from_slice(&bytes[start..end]);
            let chunk = &bytes[start..end];
            if kitty_chunk_has_more(chunk) {
                let mut next_start = end;
                loop {
                    if let Some((next_end, _)) = find_single_kitty_chunk(bytes, next_start) {
                        data.extend_from_slice(&bytes[next_start..next_end]);
                        if !kitty_chunk_has_more(&bytes[next_start..next_end]) {
                            return Some((next_end, data));
                        }
                        next_start = next_end;
                    } else {
                        return Some((end, bytes[start..end].to_vec()));
                    }
                }
            }
            return Some((end, data));
        }
        i += 1;
    }
}

fn find_single_kitty_chunk(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    if start + 2 >= bytes.len() || bytes[start] != 0x1b || bytes[start + 1] != b'_' {
        return None;
    }
    let mut i = start + 2;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            let end = i + 2;
            return Some((end, bytes[start..end].to_vec()));
        }
        i += 1;
    }
    None
}

fn kitty_chunk_has_more(chunk: &[u8]) -> bool {
    let s = String::from_utf8_lossy(chunk);
    if let Some(rest) = s.strip_prefix("\x1b_G") {
        let control = if let Some(semi_pos) = rest.find(';') {
            &rest[..semi_pos]
        } else if let Some(esc_pos) = rest.find('\x1b') {
            &rest[..esc_pos]
        } else {
            rest
        };
        for kv in control.split(',') {
            if let Some((key, value)) = kv.split_once('=') {
                if key == "m" && value == "1" {
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Kitty dimensions
// ---------------------------------------------------------------------------

fn parse_kitty_dimensions(data: &[u8], cell_w: usize, cell_h: usize) -> (usize, usize) {
    let s = String::from_utf8_lossy(data);
    let control = if let Some(rest) = s.strip_prefix("\x1b_G") {
        if let Some(semi_pos) = rest.find(';') {
            &rest[..semi_pos]
        } else if let Some(esc_pos) = rest.find('\x1b') {
            &rest[..esc_pos]
        } else {
            rest
        }
    } else {
        return (1, 1);
    };

    let mut cols = 0usize;
    let mut rows = 0usize;
    let mut pixel_w = 0usize;
    let mut pixel_h = 0usize;
    let mut format = 32u32;

    for kv in control.split(',') {
        if let Some((key, value)) = kv.split_once('=') {
            match key {
                "c" => cols = value.parse().unwrap_or(0),
                "r" => rows = value.parse().unwrap_or(0),
                "s" => pixel_w = value.parse().unwrap_or(0),
                "v" => pixel_h = value.parse().unwrap_or(0),
                "f" => format = value.parse().unwrap_or(32),
                _ => {}
            }
        }
    }

    if cols > 0 && rows > 0 {
        return (cols, rows);
    }

    // For PNG images without explicit pixel dimensions, read them from the
    // PNG header (IHDR chunk) in the first kitty chunk's payload.
    if pixel_w == 0 && pixel_h == 0 && format == 100 {
        if let Some((w, h)) = read_kitty_png_dimensions(data) {
            pixel_w = w;
            pixel_h = h;
        }
    }

    if pixel_w > 0 || pixel_h > 0 {
        if cols == 0 && pixel_w > 0 {
            cols = pixel_w.div_ceil(cell_w);
        }
        if rows == 0 && pixel_h > 0 {
            rows = pixel_h.div_ceil(cell_h);
        }
    }
    (cols.max(1), rows.max(1))
}

/// Read pixel dimensions from the PNG IHDR chunk inside a kitty image's payload.
/// Only needs the first chunk's payload (IHDR is always at the start).
fn read_kitty_png_dimensions(data: &[u8]) -> Option<(usize, usize)> {
    let chunks = parse_kitty_chunks(data);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&chunks.first()?.payload)
        .ok()?;
    // PNG: 8-byte signature + 4-byte length + 4-byte "IHDR" + 4-byte width + 4-byte height
    if decoded.len() < 24 || &decoded[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([decoded[16], decoded[17], decoded[18], decoded[19]]) as usize;
    let h = u32::from_be_bytes([decoded[20], decoded[21], decoded[22], decoded[23]]) as usize;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Merge & incomplete-sequence helpers
// ---------------------------------------------------------------------------

fn merge_image_lines(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut pending: Option<String> = None;

    for line in input.lines() {
        if let Some(ref mut p) = pending {
            p.push_str(line);
            if sequence_complete(p) {
                result.push(std::mem::take(p));
                pending = None;
            }
        } else if has_incomplete_sequence(line) {
            pending = Some(line.to_string());
        } else {
            result.push(line.to_string());
        }
    }

    if let Some(p) = pending {
        result.push(p);
    }
    if result.is_empty() && !input.is_empty() {
        result.push(String::new());
    }
    result
}

fn has_incomplete_sequence(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            if bytes[i + 1] == b'P' && find_sixel_end(bytes, i).is_none() {
                return true;
            }
            if bytes[i + 1] == b'_' && find_kitty_end(bytes, i).is_none() {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn sequence_complete(line: &str) -> bool {
    !has_incomplete_sequence(line)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Process all input lines: extract images, insert blank spacer lines,
/// and return the expanded line list + image list with correct line indices.
pub fn process_input(input: &str, cell_w: usize, cell_h: usize) -> (Vec<String>, Vec<InlineImage>) {
    let merged = merge_image_lines(input);

    // First pass: scan each merged line for images.
    struct LineResult {
        cleaned: String,
        images: Vec<ExtractedImage>,
    }
    let mut line_results: Vec<LineResult> = Vec::with_capacity(merged.len());
    for line in &merged {
        let scan = scan_line_for_images(line, cell_w, cell_h);
        line_results.push(LineResult {
            cleaned: scan.cleaned,
            images: scan.images,
        });
    }

    // Second pass: build expanded lines and assign final line indices.
    // For each original line that contains image(s), insert blank spacer lines
    // equal to (max_image_height - 1) so the image doesn't overlap subsequent text.
    let mut expanded_lines: Vec<String> = Vec::new();
    let mut all_images: Vec<InlineImage> = Vec::new();

    for lr in line_results {
        let current_expanded_idx = expanded_lines.len();

        // Push the cleaned text line
        expanded_lines.push(lr.cleaned);

        // Determine how many extra rows are needed for images on this line
        let max_image_height = lr
            .images
            .iter()
            .map(|img| img.height_rows)
            .max()
            .unwrap_or(0);

        for img in lr.images {
            all_images.push(InlineImage {
                line_idx: current_expanded_idx,
                col: img.col,
                height_rows: img.height_rows,
                width_cols: img.width_cols,
                data: img.data,
                protocol: img.protocol,
                sixel_row_count: img.sixel_row_count,
                sixel_data_start: img.sixel_data_start,
                sixel_row_offsets: img.sixel_row_offsets,
                sixel_color_defs: img.sixel_color_defs,
            });
        }

        // Insert blank spacer lines so the image has room
        if max_image_height > 1 {
            for _ in 0..max_image_height - 1 {
                expanded_lines.push(String::new());
            }
        }
    }

    (expanded_lines, all_images)
}

/// For a given scroll offset and viewport, return which images overlap the viewport.
pub fn visible_images(
    images: &[InlineImage],
    scroll_offset: usize,
    viewport_height: usize,
) -> Vec<&InlineImage> {
    let viewport_end = scroll_offset + viewport_height;
    images
        .iter()
        .filter(|img| {
            let img_end = img.line_idx + img.height_rows;
            img.line_idx < viewport_end && img_end > scroll_offset
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    const CELL_W: usize = 8;
    const CELL_H: usize = 16;

    fn load_sixel_fixture() -> String {
        std::fs::read_to_string("tests/fixtures/autobahn.sixel").unwrap()
    }

    /// Compare `data` against a stored binary reference file.
    /// When `UPDATE_SNAPSHOTS=1` is set (or the file doesn't exist yet),
    /// writes the data and lets the test pass so all snapshots can be
    /// generated in one run.
    fn assert_binary_snapshot(name: &str, data: &[u8]) {
        let path = format!("tests/fixtures/{}", name);
        let update = std::env::var("UPDATE_SNAPSHOTS").is_ok_and(|v| v == "1");

        if update || !std::path::Path::new(&path).exists() {
            std::fs::write(&path, data).unwrap();
            if !update {
                panic!(
                    "New binary snapshot written to {path}. \
                     Review it, then re-run the tests. \
                     To regenerate all snapshots at once: \
                     UPDATE_SNAPSHOTS=1 cargo test"
                );
            }
            return;
        }
        let expected = std::fs::read(&path).unwrap();
        if data != expected.as_slice() {
            panic!(
                "Binary snapshot mismatch for {path} \
                 (got {} bytes, expected {} bytes). \
                 To update: UPDATE_SNAPSHOTS=1 cargo test",
                data.len(),
                expected.len(),
            );
        }
    }

    /// Summarize an InlineImage for snapshotting (excluding raw data bytes).
    fn summarize_image(img: &InlineImage) -> String {
        format!(
            "line_idx: {}\n\
             col: {}\n\
             height_rows: {}\n\
             width_cols: {}\n\
             protocol: {:?}\n\
             sixel_row_count: {}\n\
             sixel_data_start: {}\n\
             sixel_row_offsets_count: {}\n\
             sixel_color_defs_count: {}\n\
             data_length: {}",
            img.line_idx,
            img.col,
            img.height_rows,
            img.width_cols,
            img.protocol,
            img.sixel_row_count,
            img.sixel_data_start,
            img.sixel_row_offsets.len(),
            img.sixel_color_defs.len(),
            img.data.len(),
        )
    }

    #[test]
    fn sixel_image_processing() {
        let input = load_sixel_fixture();
        let (lines, images) = process_input(&input, CELL_W, CELL_H);

        let mut summary = format!(
            "expanded_lines: {}\nimages: {}\n",
            lines.len(),
            images.len()
        );
        for (i, img) in images.iter().enumerate() {
            summary.push_str(&format!(
                "\n--- Image {} ---\n{}\n",
                i,
                summarize_image(img)
            ));
        }

        assert_snapshot!(summary);
    }

    #[test]
    fn sixel_no_clip_needed() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let result = clip_sixel(img, 0, img.height_rows + 10, CELL_H);
        let clipped = result.expect("should return Some (fast path: clone)");

        assert_eq!(
            clipped.len(),
            img.data.len(),
            "unclipped should match original size"
        );
        assert_eq!(clipped, img.data, "unclipped data should be identical");
    }

    #[test]
    fn sixel_clip_top_half_scrolled_off() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let skip_top = img.height_rows / 2;
        let keep_rows = img.height_rows;

        let clipped = clip_sixel(img, skip_top, keep_rows, CELL_H)
            .expect("clip should return Some for partial visibility");

        assert_binary_snapshot("autobahn-clip-top-half.sixel", &clipped);
    }

    #[test]
    fn sixel_clip_bottom_cropped() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_sixel(img, 0, img.height_rows / 2, CELL_H)
            .expect("clip should return Some for bottom crop");

        assert_binary_snapshot("autobahn-clip-bottom-half.sixel", &clipped);
    }

    #[test]
    fn sixel_clip_middle_visible() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_sixel(img, img.height_rows / 4, img.height_rows / 2, CELL_H)
            .expect("clip should return Some for middle visibility");

        assert_binary_snapshot("autobahn-clip-middle.sixel", &clipped);
    }

    #[test]
    fn sixel_clip_single_row_visible() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_sixel(img, img.height_rows - 1, 1, CELL_H)
            .expect("clip should return Some for single row");

        assert_binary_snapshot("autobahn-clip-single-row.sixel", &clipped);
    }

    /// Simulates the scenario where a sixel image is at the bottom of a file
    /// and only `available` terminal rows are visible.  This is the bottom-crop
    /// case (skip_top=0, keep_rows=available).
    #[test]
    fn sixel_clip_first_row_entering_viewport() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        // Only 1 terminal row of the image is visible (image just entered viewport)
        let clipped =
            clip_sixel(img, 0, 1, CELL_H).expect("clip should return Some for first visible row");

        // The clipped sixel must fit within 1 terminal row = CELL_H pixels.
        // Re-analyze the output to verify it doesn't overflow.
        let info = analyze_sixel(&clipped, CELL_W, CELL_H);
        let actual_pixels = info.sixel_row_count * 6;
        assert!(
            actual_pixels <= CELL_H,
            "clipped data has {} pixels ({} sixel rows × 6) but only {} pixels \
             available (1 row × cell_h={})",
            actual_pixels,
            info.sixel_row_count,
            CELL_H,
            CELL_H,
        );

        assert_binary_snapshot("autobahn-clip-first-row.sixel", &clipped);
    }

    /// Same as above but with a few more rows visible — the common case when
    /// scrolling an image into view with half-page or page-down.
    #[test]
    fn sixel_clip_entering_viewport_5_rows() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let available = 5;
        let clipped = clip_sixel(img, 0, available, CELL_H).expect("clip should return Some");

        let info = analyze_sixel(&clipped, CELL_W, CELL_H);
        let actual_pixels = info.sixel_row_count * 6;
        let max_pixels = available * CELL_H;
        assert!(
            actual_pixels <= max_pixels,
            "clipped data has {} pixels ({} sixel rows × 6) but only {} pixels \
             available ({} rows × cell_h={})",
            actual_pixels,
            info.sixel_row_count,
            max_pixels,
            available,
            CELL_H,
        );

        assert_binary_snapshot("autobahn-clip-entering-5-rows.sixel", &clipped);
    }

    /// Verify the top-clip direction doesn't leak pixels from scrolled-off rows.
    #[test]
    fn sixel_clip_last_row_leaving_viewport() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        // Image scrolled so only the last row is visible (top 47 rows clipped).
        let skip_top = img.height_rows - 1;
        let clipped = clip_sixel(img, skip_top, img.height_rows, CELL_H)
            .expect("clip should return Some for last visible row");

        let info = analyze_sixel(&clipped, CELL_W, CELL_H);
        let actual_pixels = info.sixel_row_count * 6;
        assert!(
            actual_pixels <= CELL_H,
            "clipped data has {} pixels but only {} available",
            actual_pixels,
            CELL_H,
        );

        assert_binary_snapshot("autobahn-clip-last-row.sixel", &clipped);
    }

    #[test]
    fn sixel_fully_scrolled_past() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let result = clip_sixel(img, img.height_rows, 10, CELL_H);
        assert!(
            result.is_none(),
            "should return None when fully scrolled past"
        );

        let result2 = clip_sixel(img, img.height_rows + 100, 10, CELL_H);
        assert!(result2.is_none(), "should return None when far past");
    }

    #[test]
    fn sixel_clip_zero_keep_rows() {
        let input = load_sixel_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let result = clip_sixel(img, 0, 0, CELL_H);
        assert!(result.is_none(), "should return None with keep_rows=0");
    }

    #[test]
    fn visible_images_at_various_offsets() {
        let input = load_sixel_fixture();
        let (lines, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];
        let viewport_height = 24;

        let offsets = [
            0,
            img.height_rows / 4,
            img.height_rows / 2,
            img.height_rows.saturating_sub(1),
            img.height_rows,
            img.height_rows + 1,
            lines.len().saturating_sub(viewport_height),
        ];

        let mut summary = format!(
            "total_lines: {}\nimage_height_rows: {}\nviewport_height: {}\n\n",
            lines.len(),
            img.height_rows,
            viewport_height
        );
        for offset in offsets {
            let visible = visible_images(&images, offset, viewport_height);
            summary.push_str(&format!(
                "scroll_offset={}: {} visible image(s)\n",
                offset,
                visible.len(),
            ));
        }

        assert_snapshot!(summary);
    }

    // -----------------------------------------------------------------------
    // Integration: reproduce the demo-file scenario
    // -----------------------------------------------------------------------

    /// Reproduce the exact scenario from the bug report: README.md text
    /// followed by a sixel image, scrolled so the first image line just
    /// enters the viewport.  Exercises the same logic as render_images().
    #[test]
    fn demo_file_image_entering_viewport() {
        let readme = std::fs::read_to_string("README.md").unwrap();
        let sixel = std::fs::read_to_string("tests/fixtures/autobahn.sixel").unwrap();
        let input = format!("{}\n{}", readme, sixel);

        // Try several plausible cell sizes (the exact value depends on the
        // user's terminal font).
        for cell_h in [16, 20, 24, 28, 32] {
            let cell_w = 8usize;
            let (_lines, images) = process_input(&input, cell_w, cell_h);
            assert_eq!(images.len(), 1, "cell_h={cell_h}: expected 1 image");
            let img = &images[0];

            let viewport_height = 44usize; // typical terminal

            // Simulate render_images() logic at the critical scroll offset
            // where the image's first line is the last viewport row.
            let scroll_offset = img.line_idx.saturating_sub(viewport_height - 1);
            let vis = visible_images(&images, scroll_offset, viewport_height);
            assert_eq!(vis.len(), 1, "cell_h={cell_h}: image should be visible");

            let skip_top = scroll_offset.saturating_sub(img.line_idx);
            let viewport_row = img.line_idx.saturating_sub(scroll_offset);
            let available = viewport_height.saturating_sub(viewport_row);

            assert_eq!(skip_top, 0, "cell_h={cell_h}");
            assert_eq!(viewport_row, viewport_height - 1, "cell_h={cell_h}");
            assert_eq!(available, 1, "cell_h={cell_h}");

            let needs_clip = skip_top > 0 || img.height_rows > available;
            assert!(
                needs_clip,
                "cell_h={cell_h}: needs_clip must be true! \
                 height_rows={} available={}",
                img.height_rows, available,
            );

            let clipped = clip_sixel(img, skip_top, available, cell_h)
                .unwrap_or_else(|| panic!("cell_h={cell_h}: clip_sixel must return Some"));

            // The critical check: the clipped data must not exceed 1 terminal
            // row of pixels.  If it does, the terminal will push the status
            // bar off-screen.
            let info = analyze_sixel(&clipped, cell_w, cell_h);
            let actual_pixels = info.sixel_row_count * 6;
            assert!(
                actual_pixels <= cell_h,
                "cell_h={cell_h}: clipped sixel has {actual_pixels}px \
                 ({} sixel rows) but only {cell_h}px available in 1 row",
                info.sixel_row_count,
            );

            // Also verify the output is much smaller than the original.
            assert!(
                clipped.len() < img.data.len() / 2,
                "cell_h={cell_h}: clipped output ({} bytes) should be much \
                 smaller than original ({} bytes)",
                clipped.len(),
                img.data.len(),
            );
        }
    }

    // -----------------------------------------------------------------------
    // ANSI clipping tests
    // -----------------------------------------------------------------------

    fn load_ansi_fixture() -> String {
        std::fs::read_to_string("tests/fixtures/autobahn.ansi").unwrap()
    }

    /// Collect lines visible in a viewport and join them back into raw ANSI
    /// output (one line per terminal row, separated by `\n`).
    fn viewport_slice(lines: &[String], scroll_offset: usize, viewport_height: usize) -> Vec<u8> {
        let end = (scroll_offset + viewport_height).min(lines.len());
        lines[scroll_offset..end].join("\n").into_bytes()
    }

    #[test]
    fn ansi_processing() {
        let input = load_ansi_fixture();
        let (lines, images) = process_input(&input, CELL_W, CELL_H);

        let summary = format!(
            "expanded_lines: {}\nimages: {}\n\
             first_line_bytes: {}\nlast_line_bytes: {}",
            lines.len(),
            images.len(),
            lines.first().map_or(0, |l| l.len()),
            lines.last().map_or(0, |l| l.len()),
        );

        assert_snapshot!(summary);
    }

    #[test]
    fn ansi_clip_top_half_scrolled_off() {
        let input = load_ansi_fixture();
        let (lines, _) = process_input(&input, CELL_W, CELL_H);
        let mid = lines.len() / 2;
        let clipped = viewport_slice(&lines, mid, lines.len());
        assert_binary_snapshot("autobahn-clip-top-half.ansi", &clipped);
    }

    #[test]
    fn ansi_clip_bottom_cropped() {
        let input = load_ansi_fixture();
        let (lines, _) = process_input(&input, CELL_W, CELL_H);
        let mid = lines.len() / 2;
        let clipped = viewport_slice(&lines, 0, mid);
        assert_binary_snapshot("autobahn-clip-bottom-half.ansi", &clipped);
    }

    #[test]
    fn ansi_clip_middle_visible() {
        let input = load_ansi_fixture();
        let (lines, _) = process_input(&input, CELL_W, CELL_H);
        let quarter = lines.len() / 4;
        let clipped = viewport_slice(&lines, quarter, lines.len() / 2);
        assert_binary_snapshot("autobahn-clip-middle.ansi", &clipped);
    }

    #[test]
    fn ansi_clip_single_row_visible() {
        let input = load_ansi_fixture();
        let (lines, _) = process_input(&input, CELL_W, CELL_H);
        let clipped = viewport_slice(&lines, lines.len() - 1, 1);
        assert_binary_snapshot("autobahn-clip-single-row.ansi", &clipped);
    }

    // -----------------------------------------------------------------------
    // Kitty clipping tests
    // -----------------------------------------------------------------------

    fn load_kitty_fixture() -> String {
        std::fs::read_to_string("tests/fixtures/autobahn.kitty").unwrap()
    }

    /// Decode a clipped kitty sequence back to raw pixels for verification.
    fn decode_kitty_to_pixels(data: &[u8]) -> (Vec<u8>, usize, usize) {
        let chunks = parse_kitty_chunks(data);
        let ctrl = parse_kitty_control(&chunks[0].control);
        let b64_cat: Vec<u8> = chunks
            .iter()
            .flat_map(|c| c.payload.iter().copied())
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64_cat)
            .unwrap();

        if ctrl.format == 100 {
            let (px, w, h, _, _) = decode_png(&decoded).unwrap();
            (px, w, h)
        } else {
            let bpp: usize = if ctrl.format == 24 { 3 } else { 4 };
            let raw = if ctrl.compressed {
                use flate2::read::ZlibDecoder;
                use std::io::Read;
                let mut dec = ZlibDecoder::new(decoded.as_slice());
                let mut out = Vec::new();
                dec.read_to_end(&mut out).unwrap();
                out
            } else {
                decoded
            };
            let w = ctrl.pixel_w;
            let h = raw.len() / (w * bpp);
            (raw, w, h)
        }
    }

    #[test]
    fn kitty_image_processing() {
        let input = load_kitty_fixture();
        let (lines, images) = process_input(&input, CELL_W, CELL_H);

        let mut summary = format!(
            "expanded_lines: {}\nimages: {}\n",
            lines.len(),
            images.len()
        );
        for (i, img) in images.iter().enumerate() {
            summary.push_str(&format!(
                "\n--- Image {} ---\n{}\n",
                i,
                summarize_image(img)
            ));
        }

        assert_snapshot!(summary);
    }

    #[test]
    fn kitty_no_clip_needed() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let result = clip_kitty(img, 0, img.height_rows + 10, CELL_H);
        let clipped = result.expect("should return Some (fast path: clone)");

        assert_eq!(
            clipped.len(),
            img.data.len(),
            "unclipped should match original size"
        );
        assert_eq!(clipped, img.data, "unclipped data should be identical");
    }

    #[test]
    fn kitty_clip_top_half_scrolled_off() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let skip_top = img.height_rows / 2;
        let keep_rows = img.height_rows;

        let clipped = clip_kitty(img, skip_top, keep_rows, CELL_H)
            .expect("clip should return Some for partial visibility");

        let (_, w, h) = decode_kitty_to_pixels(&clipped);
        // The pixel height may be slightly less than (height_rows - skip_top) * CELL_H
        // because height_rows rounds up via div_ceil.
        let max_h = (img.height_rows - skip_top) * CELL_H;
        assert!(
            h > 0 && h <= max_h,
            "pixel height {h} out of range 1..={max_h}"
        );
        assert!(w > 0, "pixel width should be nonzero");

        assert_binary_snapshot("autobahn-clip-top-half.kitty", &clipped);
    }

    #[test]
    fn kitty_clip_bottom_cropped() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_kitty(img, 0, img.height_rows / 2, CELL_H)
            .expect("clip should return Some for bottom crop");

        assert_binary_snapshot("autobahn-clip-bottom-half.kitty", &clipped);
    }

    #[test]
    fn kitty_clip_middle_visible() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_kitty(img, img.height_rows / 4, img.height_rows / 2, CELL_H)
            .expect("clip should return Some for middle visibility");

        assert_binary_snapshot("autobahn-clip-middle.kitty", &clipped);
    }

    #[test]
    fn kitty_clip_single_row_visible() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_kitty(img, img.height_rows - 1, 1, CELL_H)
            .expect("clip should return Some for single row");

        assert_binary_snapshot("autobahn-clip-single-row.kitty", &clipped);
    }

    #[test]
    fn kitty_clip_first_row_entering_viewport() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped =
            clip_kitty(img, 0, 1, CELL_H).expect("clip should return Some for first visible row");

        let (_, _, h) = decode_kitty_to_pixels(&clipped);
        assert!(
            h <= CELL_H,
            "clipped data has {} pixel rows but only {} pixels available (1 row × cell_h={})",
            h,
            CELL_H,
            CELL_H,
        );

        assert_binary_snapshot("autobahn-clip-first-row.kitty", &clipped);
    }

    #[test]
    fn kitty_clip_entering_viewport_5_rows() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let available = 5;
        let clipped = clip_kitty(img, 0, available, CELL_H).expect("clip should return Some");

        let (_, _, h) = decode_kitty_to_pixels(&clipped);
        let max_pixels = available * CELL_H;
        assert!(
            h <= max_pixels,
            "clipped data has {} pixel rows but only {} pixels available ({} rows × cell_h={})",
            h,
            max_pixels,
            available,
            CELL_H,
        );

        assert_binary_snapshot("autobahn-clip-entering-5-rows.kitty", &clipped);
    }

    #[test]
    fn kitty_clip_last_row_leaving_viewport() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let skip_top = img.height_rows - 1;
        let clipped = clip_kitty(img, skip_top, img.height_rows, CELL_H)
            .expect("clip should return Some for last visible row");

        let (_, _, h) = decode_kitty_to_pixels(&clipped);
        assert!(
            h <= CELL_H,
            "clipped data has {} pixel rows but only {} available",
            h,
            CELL_H,
        );

        assert_binary_snapshot("autobahn-clip-last-row.kitty", &clipped);
    }

    #[test]
    fn kitty_fully_scrolled_past() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let result = clip_kitty(img, img.height_rows, 10, CELL_H);
        assert!(
            result.is_none(),
            "should return None when fully scrolled past"
        );

        let result2 = clip_kitty(img, img.height_rows + 100, 10, CELL_H);
        assert!(result2.is_none(), "should return None when far past");
    }

    #[test]
    fn kitty_clip_zero_keep_rows() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let result = clip_kitty(img, 0, 0, CELL_H);
        assert!(result.is_none(), "should return None with keep_rows=0");
    }

    #[test]
    fn kitty_clip_reduces_size() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let clipped = clip_kitty(img, 0, 1, CELL_H).expect("should return Some");
        assert!(
            clipped.len() < img.data.len() / 2,
            "clipped output ({} bytes) should be much smaller than original ({} bytes)",
            clipped.len(),
            img.data.len(),
        );
    }

    #[test]
    fn kitty_clip_preserves_pixel_content() {
        let input = load_kitty_fixture();
        let (_, images) = process_input(&input, CELL_W, CELL_H);
        let img = &images[0];

        let (orig_px, orig_w, orig_h) = decode_kitty_to_pixels(&img.data);

        let skip = 5;
        let keep = 5;
        let clipped = clip_kitty(img, skip, keep, CELL_H).expect("should return Some");
        let (clip_px, clip_w, clip_h) = decode_kitty_to_pixels(&clipped);

        assert_eq!(clip_w, orig_w);
        // Use actual decoded height, not height_rows * CELL_H (which rounds up)
        let bpp = orig_px.len() / (orig_w * orig_h);
        let stride = orig_w * bpp;
        let skip_pixels = skip * CELL_H;
        let orig_slice = &orig_px[skip_pixels * stride..(skip_pixels + clip_h) * stride];
        assert_eq!(clip_px, orig_slice);
    }
}
