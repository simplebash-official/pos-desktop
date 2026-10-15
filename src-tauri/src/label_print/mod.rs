//! Direct label printing: list the computer's printers and send finished label
//! pages straight to one, with no print dialog (the system dialog stays available
//! through `printer::print_pdf_native` and the browser path).
//!
//! The frontend renders each sticker at the printer's own resolution as a
//! lossless black-and-white PNG, so what reaches the printer is exactly what the
//! person previewed. macOS and Linux hand the PNGs to CUPS (`lp`); Windows draws
//! them with GDI at the printer's resolution. All argument checking and PNG
//! decoding lives here and is shared, so the per-OS modules only talk to the OS.

mod bitmap;
#[cfg(unix)]
mod cups;
#[cfg(windows)]
mod windows_gdi;

use base64::prelude::*;
use serde_json::json;

/// The most pages one call may carry; mirrors the backend's per-print cap.
const MAX_PAGES: usize = 500;
/// Detail levels the frontend offers (dots per inch of the label printer).
const ALLOWED_DPI: [u32; 3] = [203, 300, 600];
const MAX_COPIES: u32 = 99;
const MIN_SIDE_MM: f64 = 5.0;
const MAX_SIDE_MM: f64 = 300.0;
const MAX_PRINTER_NAME_CHARS: usize = 256;

/// A validated print request: the printer, the decoded pages and the physical
/// label size they cover.
#[derive(Debug)]
pub(crate) struct PrintJob {
    pub printer: String,
    pub pages: Vec<Vec<u8>>,
    pub width_mm: f64,
    pub height_mm: f64,
    pub dpi: u32,
    pub copies: u32,
}

/// The PNG file signature; anything else is not a page we produced.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Checks every argument the frontend sent and decodes the pages. Nothing is
/// trusted: a printer name goes into a command line on some systems, and a page
/// that is not a PNG must never reach a driver.
pub(crate) fn validate(
    printer: &str,
    pages_base64: &[String],
    width_mm: f64,
    height_mm: f64,
    dpi: u32,
    copies: u32,
) -> Result<PrintJob, String> {
    let printer = printer.trim();
    if printer.is_empty() {
        return Err("Choose a printer first.".into());
    }
    if printer.chars().count() > MAX_PRINTER_NAME_CHARS || printer.chars().any(char::is_control) {
        return Err("The printer name is not valid.".into());
    }
    if pages_base64.is_empty() {
        return Err("There is nothing to print.".into());
    }
    if pages_base64.len() > MAX_PAGES {
        return Err(format!(
            "At most {MAX_PAGES} labels can be printed at a time."
        ));
    }
    for side in [width_mm, height_mm] {
        if !side.is_finite() || !(MIN_SIDE_MM..=MAX_SIDE_MM).contains(&side) {
            return Err("The label size is not valid.".into());
        }
    }
    if !ALLOWED_DPI.contains(&dpi) {
        return Err("The printer detail level is not valid.".into());
    }
    if !(1..=MAX_COPIES).contains(&copies) {
        return Err(format!("Copies must be between 1 and {MAX_COPIES}."));
    }

    let mut pages = Vec::with_capacity(pages_base64.len());
    for (index, encoded) in pages_base64.iter().enumerate() {
        let bytes = BASE64_STANDARD
            .decode(encoded.trim())
            .map_err(|e| format!("Label {} could not be read: {e}", index + 1))?;
        if !bytes.starts_with(&PNG_SIGNATURE) {
            return Err(format!("Label {} is not a PNG image.", index + 1));
        }
        pages.push(bytes);
    }
    Ok(PrintJob {
        printer: printer.to_string(),
        pages,
        width_mm,
        height_mm,
        dpi,
        copies,
    })
}

/// The printers installed on this computer, by the name the system knows them by.
#[tauri::command]
pub async fn list_printers() -> Result<Vec<String>, String> {
    let call = crate::logging::CommandLog::start("list_printers", json!({}));
    let result = tauri::async_runtime::spawn_blocking(list_printers_blocking)
        .await
        .map_err(|e| format!("Looking for printers failed: {e}"))
        .and_then(|inner| inner);
    call.finish(result)
}

/// Sends label pages straight to `printer`, `copies` times each, at a physical
/// size of `width_mm` x `height_mm`.
#[tauri::command]
pub async fn print_label_pages(
    printer: String,
    pages_base64: Vec<String>,
    width_mm: f64,
    height_mm: f64,
    dpi: u32,
    copies: u32,
) -> Result<(), String> {
    let call = crate::logging::CommandLog::start(
        "print_label_pages",
        json!({
            "printer": printer,
            "pages": pages_base64.len(),
            "width_mm": width_mm,
            "height_mm": height_mm,
            "dpi": dpi,
            "copies": copies,
        }),
    );
    let result = match validate(&printer, &pages_base64, width_mm, height_mm, dpi, copies) {
        Ok(job) => tauri::async_runtime::spawn_blocking(move || print_blocking(&job))
            .await
            .map_err(|e| format!("Printing failed: {e}"))
            .and_then(|inner| inner),
        Err(message) => Err(message),
    };
    call.finish(result)
}

#[cfg(unix)]
fn list_printers_blocking() -> Result<Vec<String>, String> {
    cups::list_printers()
}

#[cfg(windows)]
fn list_printers_blocking() -> Result<Vec<String>, String> {
    windows_gdi::list_printers()
}

#[cfg(unix)]
fn print_blocking(job: &PrintJob) -> Result<(), String> {
    cups::print(job)
}

#[cfg(windows)]
fn print_blocking(job: &PrintJob) -> Result<(), String> {
    let bitmaps = job
        .pages
        .iter()
        .enumerate()
        .map(|(index, png)| {
            bitmap::decode_png(png)
                .map_err(|e| format!("Label {} could not be read: {e}", index + 1))
        })
        .collect::<Result<Vec<_>, _>>()?;
    windows_gdi::print(job, &bitmaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_b64() -> String {
        BASE64_STANDARD.encode([&PNG_SIGNATURE[..], b"rest-of-file"].concat())
    }

    fn check(
        printer: &str,
        pages: &[String],
        w: f64,
        h: f64,
        dpi: u32,
        copies: u32,
    ) -> Result<PrintJob, String> {
        validate(printer, pages, w, h, dpi, copies)
    }

    #[test]
    fn a_good_request_is_decoded() {
        let job = check(" Zebra ZD220 ", &[png_b64(), png_b64()], 50.0, 30.0, 203, 1).unwrap();
        assert_eq!(job.printer, "Zebra ZD220");
        assert_eq!(job.pages.len(), 2);
        assert!(job.pages[0].starts_with(&PNG_SIGNATURE));
        assert_eq!(
            (job.width_mm, job.height_mm, job.dpi, job.copies),
            (50.0, 30.0, 203, 1)
        );
    }

    #[test]
    fn printer_names_are_checked() {
        for bad in ["", "   ", "a\nb", "a\0b", &"x".repeat(257)] {
            assert!(
                check(bad, &[png_b64()], 50.0, 30.0, 203, 1).is_err(),
                "{bad:?}"
            );
        }
        // Spaces, dashes and unicode are ordinary printer names.
        assert!(check("-weird name ü", &[png_b64()], 50.0, 30.0, 203, 1).is_ok());
    }

    #[test]
    fn pages_must_be_present_few_enough_and_real_pngs() {
        assert!(check("p", &[], 50.0, 30.0, 203, 1).is_err());
        let many = vec![png_b64(); MAX_PAGES + 1];
        assert!(check("p", &many, 50.0, 30.0, 203, 1).is_err());
        assert!(check("p", &vec![png_b64(); MAX_PAGES], 50.0, 30.0, 203, 1).is_ok());
        assert!(check("p", &["not base64!!".into()], 50.0, 30.0, 203, 1).is_err());
        let jpeg = BASE64_STANDARD.encode([0xFF, 0xD8, 0xFF, 0xE0]);
        let err = check("p", &[jpeg], 50.0, 30.0, 203, 1).unwrap_err();
        assert!(err.contains("not a PNG"), "{err}");
    }

    #[test]
    fn size_detail_and_copies_are_bounded() {
        for (w, h) in [
            (4.9, 30.0),
            (50.0, 300.1),
            (f64::NAN, 30.0),
            (50.0, f64::INFINITY),
            (-1.0, 30.0),
        ] {
            assert!(check("p", &[png_b64()], w, h, 203, 1).is_err(), "{w}x{h}");
        }
        for dpi in [0, 200, 1200] {
            assert!(
                check("p", &[png_b64()], 50.0, 30.0, dpi, 1).is_err(),
                "{dpi}"
            );
        }
        for dpi in [203, 300, 600] {
            assert!(
                check("p", &[png_b64()], 50.0, 30.0, dpi, 1).is_ok(),
                "{dpi}"
            );
        }
        for copies in [0, 100] {
            assert!(
                check("p", &[png_b64()], 50.0, 30.0, 203, copies).is_err(),
                "{copies}"
            );
        }
        assert!(check("p", &[png_b64()], 50.0, 30.0, 203, 99).is_ok());
    }
}
