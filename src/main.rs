#[cfg(feature = "fastest")]
use fast_image_resize::ResizeAlg;
#[cfg(any(feature = "quality", feature = "faster", feature = "fastest"))]
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use image::ImageReader;
use std::env;
use std::ffi::OsString;
use std::fs::File;
#[cfg(feature = "low-rss")]
use std::io::SeekFrom;
use std::io::{self, BufReader, Cursor, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_ulong};
use std::process::ExitCode;

#[cfg(not(any(
    feature = "quality",
    feature = "low-rss",
    feature = "faster",
    feature = "fastest"
)))]
compile_error!("enable exactly one of the quality, low-rss, faster, or fastest features");
#[cfg(any(
    all(feature = "quality", feature = "low-rss"),
    all(feature = "quality", feature = "faster"),
    all(feature = "quality", feature = "fastest"),
    all(feature = "low-rss", feature = "faster"),
    all(feature = "low-rss", feature = "fastest"),
    all(feature = "faster", feature = "fastest")
))]
compile_error!("the quality, low-rss, faster, and fastest features are mutually exclusive");

const RAW_CHUNK_SIZE: usize = 3072;
const ENCODED_CHUNK_SIZE: usize = 4096;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const HELP: &str = "Usage: kitkat-rs IMAGE_FILE\n       kitkat-rs - < IMAGE_FILE\n\nDisplay a PNG or JPEG using the Kitty graphics protocol.\nUse - to read the image from standard input.\n";

const TIOCGWINSZ: c_ulong = 0x5413;

#[repr(C)]
#[derive(Default)]
struct Winsize {
    rows: u16,
    columns: u16,
    pixel_width: u16,
    pixel_height: u16,
}

#[derive(Clone, Copy)]
struct TerminalGeometry {
    rows: u16,
    columns: u16,
    pixel_width: u16,
    pixel_height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Placement {
    rows: u32,
    columns: u32,
    left_cells: u32,
    left_pixels: u32,
}

struct Raster {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    format: u8,
    placement: Placement,
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("kitkat-rs: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: impl Iterator<Item = OsString>) -> io::Result<()> {
    let Some(path) = parse_args(args)? else {
        print!("{HELP}");
        return Ok(());
    };
    let tmux = env::var_os("TMUX").is_some();
    let stdout = io::stdout();
    let geometry = terminal_geometry(stdout.as_raw_fd());
    let mut output = stdout.lock();

    if path == "-" {
        let mut compressed = Vec::new();
        io::stdin().lock().read_to_end(&mut compressed)?;
        transmit_image(Cursor::new(compressed), &mut output, tmux, geometry)
    } else {
        transmit_image(File::open(path)?, &mut output, tmux, geometry)
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> io::Result<Option<OsString>> {
    let Some(argument) = args.next() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, HELP));
    };

    if argument == "-h" || argument == "--help" {
        return Ok(None);
    }
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected exactly one image file",
        ));
    }
    Ok(Some(argument))
}

fn terminal_geometry(fd: c_int) -> TerminalGeometry {
    query_terminal_geometry(fd)
        .or_else(|| {
            File::open("/dev/tty")
                .ok()
                .and_then(|terminal| query_terminal_geometry(terminal.as_raw_fd()))
        })
        .unwrap_or(TerminalGeometry {
            rows: 24,
            columns: 80,
            pixel_width: 640,
            pixel_height: 384,
        })
}

fn query_terminal_geometry(fd: c_int) -> Option<TerminalGeometry> {
    let mut size = Winsize::default();
    // SAFETY: TIOCGWINSZ only writes one Winsize value through this valid pointer.
    (unsafe { ioctl(fd, TIOCGWINSZ, &mut size) } == 0 && size.rows != 0 && size.columns != 0)
        .then_some(TerminalGeometry {
            rows: size.rows,
            columns: size.columns,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        })
}

fn transmit_image(
    input: impl Read + Seek,
    mut output: impl Write,
    tmux: bool,
    geometry: TerminalGeometry,
) -> io::Result<()> {
    let raster = decode_image(input, geometry)?;
    reserve_rows(&mut output, raster.placement)?;

    let mut input = raster.pixels.as_slice();
    let mut current = [0; RAW_CHUNK_SIZE];
    let mut next = [0; RAW_CHUNK_SIZE];
    let mut current_len = fill_chunk(&mut input, &mut current)?;
    let mut first = true;

    while current_len != 0 {
        let next_len = fill_chunk(&mut input, &mut next)?;
        write_chunk(
            &mut output,
            &current[..current_len],
            first,
            next_len != 0,
            tmux,
            &raster,
        )?;
        first = false;
        std::mem::swap(&mut current, &mut next);
        current_len = next_len;
    }

    write!(output, "\x1b[{}E", raster.placement.rows)?;
    output.flush()
}

#[cfg(any(feature = "quality", feature = "faster", feature = "fastest"))]
fn decode_image(input: impl Read + Seek, geometry: TerminalGeometry) -> io::Result<Raster> {
    let mut reader = ImageReader::new(BufReader::new(input))
        .with_guessed_format()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot identify image: {error}"),
            )
        })?;
    reader.no_limits();
    let image = reader.decode().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot decode image: {error}"),
        )
    })?;
    let (width, height, placement) = fit_image(image.width(), image.height(), geometry);
    let source_width = image.width();
    let source_height = image.height();
    let (pixels, pixel_type, format) = if image.color().has_alpha() {
        (image.into_rgba8().into_raw(), PixelType::U8x4, 32)
    } else {
        (image.into_rgb8().into_raw(), PixelType::U8x3, 24)
    };
    let source = fast_image_resize::images::Image::from_vec_u8(
        source_width,
        source_height,
        pixels,
        pixel_type,
    )
    .map_err(|error| io::Error::other(format!("cannot prepare image: {error}")))?;
    let mut resized = fast_image_resize::images::Image::new(width, height, pixel_type);
    let options = ResizeOptions::new();
    #[cfg(feature = "fastest")]
    let options = options.resize_alg(ResizeAlg::Nearest);
    Resizer::new()
        .resize(&source, &mut resized, &options)
        .map_err(|error| io::Error::other(format!("cannot resize image: {error}")))?;

    Ok(Raster {
        pixels: resized.into_vec(),
        width,
        height,
        format,
        placement,
    })
}

#[cfg(all(
    feature = "low-rss",
    not(any(feature = "quality", feature = "faster", feature = "fastest"))
))]
fn decode_image(mut input: impl Read + Seek, geometry: TerminalGeometry) -> io::Result<Raster> {
    let mut signature = [0; 8];
    let signature_len = input.read(&mut signature)?;
    input.seek(SeekFrom::Start(0))?;
    if signature_len == signature.len() && signature == *b"\x89PNG\r\n\x1a\n" {
        return decode_png_streaming(input, geometry);
    }

    let mut reader = ImageReader::new(BufReader::new(input))
        .with_guessed_format()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot identify image: {error}"),
            )
        })?;
    reader.no_limits();
    let image = reader.decode().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot decode image: {error}"),
        )
    })?;
    let source_width = image.width();
    let source_height = image.height();
    let (width, height, placement) = fit_image(source_width, source_height, geometry);
    let (pixels, channels, format) = if image.color().has_alpha() {
        (image.into_rgba8().into_raw(), 4, 32)
    } else {
        (image.into_rgb8().into_raw(), 3, 24)
    };

    Ok(Raster {
        pixels: resize_packed_nearest(pixels, source_width, source_height, width, height, channels),
        width,
        height,
        format,
        placement,
    })
}

#[cfg(feature = "low-rss")]
fn decode_png_streaming(input: impl Read + Seek, geometry: TerminalGeometry) -> io::Result<Raster> {
    let mut decoder =
        png::Decoder::new_with_limits(BufReader::new(input), png::Limits { bytes: usize::MAX });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(png_error)?;
    let source_width = reader.info().width;
    let source_height = reader.info().height;
    let interlaced = reader.info().interlaced;
    let (color_type, bit_depth) = reader.output_color_type();
    if bit_depth != png::BitDepth::Eight {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported PNG bit depth: {bit_depth:?}"),
        ));
    }
    let format = match color_type {
        png::ColorType::Rgb | png::ColorType::Grayscale => 24,
        png::ColorType::Rgba | png::ColorType::GrayscaleAlpha => 32,
        png::ColorType::Indexed => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "indexed PNG was not expanded",
            ));
        }
    };
    let (width, height, placement) = fit_image(source_width, source_height, geometry);
    let output_channels = usize::from(format / 8);
    let mut pixels = Vec::with_capacity(width as usize * height as usize * output_channels);

    if interlaced {
        let buffer_size = reader
            .output_buffer_size()
            .ok_or_else(|| io::Error::other("PNG output is too large"))?;
        let mut decoded = vec![0; buffer_size];
        let info = reader.next_frame(&mut decoded).map_err(png_error)?;
        for target_y in 0..height {
            let source_y = nearest_coordinate(target_y, height, source_height) as usize;
            let row_start = source_y * info.line_size;
            push_sampled_png_row(
                &mut pixels,
                &decoded[row_start..row_start + info.line_size],
                source_width,
                width,
                color_type,
            );
        }
    } else {
        let mut target_y = 0;
        for source_y in 0..source_height {
            let row = reader
                .next_row()
                .map_err(png_error)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing PNG row"))?;
            while target_y < height
                && nearest_coordinate(target_y, height, source_height) == source_y
            {
                push_sampled_png_row(&mut pixels, row.data(), source_width, width, color_type);
                target_y += 1;
            }
        }
        if target_y != height {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "missing sampled PNG row",
            ));
        }
    }

    Ok(Raster {
        pixels,
        width,
        height,
        format,
        placement,
    })
}

#[cfg(feature = "low-rss")]
fn png_error(error: png::DecodingError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("cannot decode PNG: {error}"),
    )
}

#[cfg(feature = "low-rss")]
fn nearest_coordinate(target: u32, target_length: u32, source_length: u32) -> u32 {
    ((((u64::from(target) * 2 + 1) * u64::from(source_length)) / (u64::from(target_length) * 2))
        as u32)
        .min(source_length - 1)
}

#[cfg(feature = "low-rss")]
fn push_sampled_png_row(
    output: &mut Vec<u8>,
    row: &[u8],
    source_width: u32,
    target_width: u32,
    color_type: png::ColorType,
) {
    let channels = color_type.samples();
    for target_x in 0..target_width {
        let source_x = nearest_coordinate(target_x, target_width, source_width) as usize;
        let pixel = &row[source_x * channels..][..channels];
        match color_type {
            png::ColorType::Rgb | png::ColorType::Rgba => output.extend_from_slice(pixel),
            png::ColorType::Grayscale => output.extend_from_slice(&[pixel[0]; 3]),
            png::ColorType::GrayscaleAlpha => {
                output.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            png::ColorType::Indexed => unreachable!(),
        }
    }
}

#[cfg(feature = "low-rss")]
fn resize_packed_nearest(
    pixels: Vec<u8>,
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    channels: usize,
) -> Vec<u8> {
    if (source_width, source_height) == (target_width, target_height) {
        return pixels;
    }
    let mut resized = Vec::with_capacity(target_width as usize * target_height as usize * channels);
    for target_y in 0..target_height {
        let source_y = nearest_coordinate(target_y, target_height, source_height) as usize;
        for target_x in 0..target_width {
            let source_x = nearest_coordinate(target_x, target_width, source_width) as usize;
            let start = (source_y * source_width as usize + source_x) * channels;
            resized.extend_from_slice(&pixels[start..start + channels]);
        }
    }
    resized
}

fn fit_image(
    source_width: u32,
    source_height: u32,
    geometry: TerminalGeometry,
) -> (u32, u32, Placement) {
    let cell_width = if geometry.pixel_width == 0 {
        8
    } else {
        u64::from(geometry.pixel_width / geometry.columns).max(1)
    };
    let cell_height = if geometry.pixel_height == 0 {
        16
    } else {
        u64::from(geometry.pixel_height / geometry.rows).max(1)
    };
    let max_width = u64::from(geometry.columns).max(1) * cell_width;
    let max_height = u64::from(geometry.rows.saturating_sub(1)).max(1) * cell_height;
    let mut width = u64::from(source_width);
    let mut height = u64::from(source_height);

    if width > max_width {
        height = div_ceil(height * max_width, width);
        width = max_width;
    }
    if height > max_height {
        width = div_ceil(width * max_height, height);
        height = max_height;
    }

    let columns = div_ceil(width, cell_width);
    let placement = Placement {
        rows: div_ceil(height, cell_height) as u32,
        columns: columns as u32,
        left_cells: (u64::from(geometry.columns).saturating_sub(columns) / 2) as u32,
        left_pixels: if width % cell_width == 0 {
            0
        } else {
            ((cell_width - width % cell_width) / 2) as u32
        },
    };
    (width as u32, height as u32, placement)
}

fn div_ceil(value: u64, divisor: u64) -> u64 {
    value / divisor + u64::from(value % divisor != 0)
}

fn reserve_rows(output: &mut impl Write, placement: Placement) -> io::Result<()> {
    for _ in 0..placement.rows {
        output.write_all(b"\n")?;
    }
    write!(output, "\x1b[{}F", placement.rows)?;
    if placement.left_cells != 0 {
        write!(output, "\x1b[{}C", placement.left_cells)?;
    }
    Ok(())
}

fn fill_chunk(input: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut length = 0;
    while length < buffer.len() {
        match input.read(&mut buffer[length..]) {
            Ok(0) => break,
            Ok(read) => length += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(length)
}

fn write_chunk(
    output: &mut impl Write,
    raw: &[u8],
    first: bool,
    more: bool,
    tmux: bool,
    raster: &Raster,
) -> io::Result<()> {
    let mut encoded = [0; ENCODED_CHUNK_SIZE];
    let encoded_len = encode_base64(raw, &mut encoded);

    if tmux {
        output.write_all(b"\x1bPtmux;\x1b\x1b_G")?;
    } else {
        output.write_all(b"\x1b_G")?;
    }

    write!(output, "m={}", u8::from(more))?;
    if first {
        write!(
            output,
            ",a=T,f={},s={},v={},X={},C=1",
            raster.format, raster.width, raster.height, raster.placement.left_pixels
        )?;
    }
    output.write_all(b";")?;
    output.write_all(&encoded[..encoded_len])?;

    if tmux {
        output.write_all(b"\x1b\x1b\\\x1b\\")
    } else {
        output.write_all(b"\x1b\\")
    }
}

fn encode_base64(input: &[u8], output: &mut [u8; ENCODED_CHUNK_SIZE]) -> usize {
    let mut source = 0;
    let mut target = 0;

    while source + 3 <= input.len() {
        let bits = u32::from(input[source]) << 16
            | u32::from(input[source + 1]) << 8
            | u32::from(input[source + 2]);
        output[target] = BASE64[((bits >> 18) & 0x3f) as usize];
        output[target + 1] = BASE64[((bits >> 12) & 0x3f) as usize];
        output[target + 2] = BASE64[((bits >> 6) & 0x3f) as usize];
        output[target + 3] = BASE64[(bits & 0x3f) as usize];
        source += 3;
        target += 4;
    }

    match input.len() - source {
        1 => {
            let bits = u32::from(input[source]) << 16;
            output[target] = BASE64[((bits >> 18) & 0x3f) as usize];
            output[target + 1] = BASE64[((bits >> 12) & 0x3f) as usize];
            output[target + 2] = b'=';
            output[target + 3] = b'=';
            target += 4;
        }
        2 => {
            let bits = u32::from(input[source]) << 16 | u32::from(input[source + 1]) << 8;
            output[target] = BASE64[((bits >> 18) & 0x3f) as usize];
            output[target + 1] = BASE64[((bits >> 12) & 0x3f) as usize];
            output[target + 2] = BASE64[((bits >> 6) & 0x3f) as usize];
            output[target + 3] = b'=';
            target += 4;
        }
        _ => {}
    }

    target
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    fn geometry() -> TerminalGeometry {
        TerminalGeometry {
            rows: 24,
            columns: 80,
            pixel_width: 640,
            pixel_height: 384,
        }
    }

    fn rgba_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        png
    }

    fn rgb_jpeg(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .write_image(pixels, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }

    #[test]
    fn sends_small_png_in_one_sequence() {
        let png = rgba_png(1, 1, &[1, 2, 3, 4]);
        let mut output = Vec::new();

        transmit_image(Cursor::new(png), &mut output, false, geometry()).unwrap();

        assert!(
            output.starts_with(b"\n\x1b[1F\x1b[39C\x1b_Gm=0,a=T,f=32,s=1,v=1,X=3,C=1;AQIDBA==")
        );
        assert!(output.ends_with(b"\x1b\\\x1b[1E"));
    }

    #[test]
    fn sends_jpeg_identified_by_content() {
        let jpeg = rgb_jpeg(1, 1, &[12, 34, 56]);
        let mut output = Vec::new();

        transmit_image(Cursor::new(jpeg), &mut output, false, geometry()).unwrap();

        assert!(output.starts_with(b"\n\x1b[1F\x1b[39C\x1b_Gm=0,a=T,f=24,s=1,v=1,X=3,C=1;"));
        assert!(output.ends_with(b"\x1b\\\x1b[1E"));
    }
    #[test]
    fn chunks_large_png_and_marks_continuation() {
        let png = rgba_png(33, 24, &vec![0x55; 33 * 24 * 4]);
        let mut output = Vec::new();

        transmit_image(Cursor::new(png), &mut output, false, geometry()).unwrap();

        assert!(output.starts_with(b"\n\n\x1b[2F\x1b[37C\x1b_Gm=1,a=T,f=32,s=33,v=24,X=3,C=1;"));
        assert_eq!(
            output
                .windows(b"\x1b_G".len())
                .filter(|part| *part == b"\x1b_G")
                .count(),
            2
        );
        assert!(output.ends_with(b"\x1b\\\x1b[2E"));
    }

    #[test]
    fn wraps_each_sequence_for_tmux() {
        let png = rgba_png(1, 1, &[1, 2, 3, 4]);
        let mut output = Vec::new();

        transmit_image(Cursor::new(png), &mut output, true, geometry()).unwrap();

        assert!(
            output
                .starts_with(b"\n\x1b[1F\x1b[39C\x1bPtmux;\x1b\x1b_Gm=0,a=T,f=32,s=1,v=1,X=3,C=1;")
        );
        assert!(output.ends_with(b"\x1b\x1b\\\x1b\\\x1b[1E"));
    }

    #[test]
    fn rejects_non_image_input() {
        let error = transmit_image(Cursor::new(b"not an image!"), Vec::new(), false, geometry())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn fits_image_inside_terminal_without_distorting_aspect_ratio() {
        assert_eq!(
            fit_image(1200, 1200, geometry()),
            (
                368,
                368,
                Placement {
                    rows: 23,
                    columns: 46,
                    left_cells: 17,
                    left_pixels: 0,
                },
            )
        );
        assert_eq!(
            fit_image(1600, 100, geometry()),
            (
                640,
                40,
                Placement {
                    rows: 3,
                    columns: 80,
                    left_cells: 0,
                    left_pixels: 0,
                },
            )
        );
        assert_eq!(
            fit_image(4, 4, geometry()),
            (
                4,
                4,
                Placement {
                    rows: 1,
                    columns: 1,
                    left_cells: 39,
                    left_pixels: 2,
                },
            )
        );
    }

    #[test]
    fn downscaling_uses_selected_filter() {
        let pixels: Vec<u8> = (0..8)
            .flat_map(|index| {
                let value = if index % 2 == 0 { 0 } else { 255 };
                [value, value, value, 255]
            })
            .collect();
        let png = rgba_png(8, 1, &pixels);
        let raster = decode_image(
            Cursor::new(png),
            TerminalGeometry {
                rows: 2,
                columns: 1,
                pixel_width: 1,
                pixel_height: 2,
            },
        )
        .unwrap();

        assert_eq!((raster.width, raster.height), (1, 1));
        #[cfg(any(feature = "quality", feature = "faster"))]
        assert!(raster.pixels[0] > 32 && raster.pixels[0] < 223);
        #[cfg(any(feature = "low-rss", feature = "fastest"))]
        assert!(matches!(raster.pixels[0], 0 | 255));
        assert_eq!(raster.pixels[3], 255);
    }

    #[test]
    fn base64_handles_padding() {
        let mut encoded = [0; ENCODED_CHUNK_SIZE];

        let length = encode_base64(b"f", &mut encoded);
        assert_eq!(&encoded[..length], b"Zg==");
        let length = encode_base64(b"fo", &mut encoded);
        assert_eq!(&encoded[..length], b"Zm8=");
        let length = encode_base64(b"foo", &mut encoded);
        assert_eq!(&encoded[..length], b"Zm9v");
    }

    #[test]
    fn parses_file_and_standard_input_arguments() {
        assert_eq!(
            parse_args([OsString::from("image.jpg")].into_iter()).unwrap(),
            Some(OsString::from("image.jpg"))
        );
        assert_eq!(
            parse_args([OsString::from("-")].into_iter()).unwrap(),
            Some(OsString::from("-"))
        );
    }

    #[test]
    fn handles_help_and_rejects_wrong_argument_counts() {
        assert_eq!(
            parse_args([OsString::from("--help")].into_iter()).unwrap(),
            None
        );
        assert_eq!(
            parse_args(Vec::<OsString>::new().into_iter())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            parse_args([OsString::from("one.png"), OsString::from("two.png")].into_iter())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
