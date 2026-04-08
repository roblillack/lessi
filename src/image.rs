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
    let skip_sixel_top = (skip_top * cell_h) / 6;
    let keep_pixels = visible_rows * cell_h;
    let keep_sixel_rows = keep_pixels.div_ceil(6);

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
            // Find the end of raster attributes
            if let Some(raster_end) = stripped.find(|c: char| {
                c == '#' || c == '!' || c == '$' || c == '-' || ('?'..='~').contains(&c)
            }) {
                let raster = &stripped[..raster_end];
                let parts: Vec<&str> = raster.split(';').collect();
                if parts.len() >= 4 {
                    // Rebuild with adjusted Pv
                    let new_raster =
                        format!("{};{};{};{}", parts[0], parts[1], parts[2], new_pixel_h);
                    let prefix = &s[..q_pos + 1]; // up to and including 'q'
                    let suffix = &stripped[raster_end..]; // after raster attrs
                    let result = format!("{}\"{}{}", prefix, new_raster, suffix);
                    return result.into_bytes();
                }
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

    for kv in control.split(',') {
        if let Some((key, value)) = kv.split_once('=') {
            match key {
                "c" => cols = value.parse().unwrap_or(0),
                "r" => rows = value.parse().unwrap_or(0),
                "s" => pixel_w = value.parse().unwrap_or(0),
                "v" => pixel_h = value.parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    if cols > 0 && rows > 0 {
        return (cols, rows);
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
