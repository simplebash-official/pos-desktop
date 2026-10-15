//! Windows: list printers with `EnumPrintersW` and print with GDI. Each label is
//! drawn 1:1 at the printer's own resolution (a page image drawn at 203 dpi onto a
//! 203 dpi printer maps one pixel to one dot), on a custom paper of exactly the
//! label size. Nothing here is covered by an automated test: it needs a Windows
//! printer driver. `bitmap.rs` and `mod.rs` hold the logic that is tested.

use std::{ffi::c_void, mem::size_of};

use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Graphics::{
            Gdi::{
                CreateDCW, DeleteDC, GetDeviceCaps, SetStretchBltMode, StretchDIBits, BITMAPINFO,
                BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DEVMODEW, DIB_RGB_COLORS, DMPAPER_USER,
                DM_IN_BUFFER, DM_OUT_BUFFER, DM_PAPERLENGTH, DM_PAPERSIZE, DM_PAPERWIDTH, HDC,
                LOGPIXELSX, LOGPIXELSY, PHYSICALOFFSETX, PHYSICALOFFSETY, RGBQUAD, SRCCOPY,
            },
            Printing::{
                ClosePrinter, DocumentPropertiesW, EnumPrintersW, OpenPrinterW,
                PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_HANDLE, PRINTER_INFO_4W,
            },
        },
        Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW},
    },
};

use super::{bitmap::Bitmap, PrintJob};

/// `DocumentPropertiesW` answers this when it filled the buffer.
const IDOK: i32 = 1;

/// Names of the printers installed here (local and network connections).
pub(super) fn list_printers() -> Result<Vec<String>, String> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let mut needed = 0u32;
    let mut returned = 0u32;
    // The first call only measures the buffer; it fails by design when there is
    // anything to list, and succeeds with `needed == 0` when there is not.
    // SAFETY: valid out-pointers, no buffer.
    let _ = unsafe { EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut returned) };
    if needed == 0 {
        return Ok(Vec::new());
    }

    // u64 storage keeps the buffer aligned for the PRINTER_INFO_4W records.
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the byte view covers exactly `needed` bytes of `storage`.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), needed as usize)
    };
    // SAFETY: the buffer is at least as large as the size reported above.
    unsafe {
        EnumPrintersW(
            flags,
            PCWSTR::null(),
            4,
            Some(bytes),
            &mut needed,
            &mut returned,
        )
        .map_err(|e| format!("Could not list the printers: {e}"))?;
    }
    // SAFETY: Windows wrote `returned` aligned PRINTER_INFO_4W records at the start.
    let records = unsafe {
        std::slice::from_raw_parts(
            storage.as_ptr().cast::<PRINTER_INFO_4W>(),
            returned as usize,
        )
    };
    let mut names: Vec<String> = records
        .iter()
        // SAFETY: each name points into the same buffer and is NUL-terminated.
        .filter_map(|record| unsafe { record.pPrinterName.to_string().ok() })
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Releases a device context on every exit path.
struct DeviceContext(HDC);

impl Drop for DeviceContext {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `CreateDCW` and is released once.
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// The printer's own settings with the paper set to a custom `width_mm` x
/// `height_mm`. The driver gets the final say (it may round or refuse a size its
/// forms do not allow), which is why the result is merged back through it.
/// Returned as aligned storage because a DEVMODE is followed by driver data.
fn label_devmode(name: &HSTRING, width_mm: f64, height_mm: f64) -> Result<Vec<u64>, String> {
    let mut handle = PRINTER_HANDLE::default();
    // SAFETY: valid name and out-pointer.
    unsafe { OpenPrinterW(name, &mut handle, None) }
        .map_err(|e| format!("Could not open the printer: {e}"))?;

    let result = (|| {
        // SAFETY: with no output buffer this only returns the size needed.
        let size = unsafe { DocumentPropertiesW(None, handle, name, None, None, 0) };
        if size <= 0 {
            return Err("The printer did not report its settings.".to_string());
        }
        let mut storage = vec![0u64; (size as usize).div_ceil(8)];
        let devmode = storage.as_mut_ptr().cast::<DEVMODEW>();

        // SAFETY: `storage` holds `size` bytes, the size the driver asked for.
        let got = unsafe {
            DocumentPropertiesW(None, handle, name, Some(devmode), None, DM_OUT_BUFFER.0)
        };
        if got != IDOK {
            return Err("The printer did not give its settings.".to_string());
        }
        // SAFETY: `devmode` points at the DEVMODEW the driver just filled in.
        unsafe {
            (*devmode).dmFields =
                (*devmode).dmFields | DM_PAPERSIZE | DM_PAPERLENGTH | DM_PAPERWIDTH;
            let paper = &mut (*devmode).Anonymous1.Anonymous1;
            paper.dmPaperSize = DMPAPER_USER as i16;
            // DEVMODE paper sizes are in tenths of a millimetre.
            paper.dmPaperWidth = (width_mm * 10.0).round() as i16;
            paper.dmPaperLength = (height_mm * 10.0).round() as i16;
        }
        // SAFETY: input and output are the same valid DEVMODE buffer.
        let merged = unsafe {
            DocumentPropertiesW(
                None,
                handle,
                name,
                Some(devmode),
                Some(devmode.cast_const()),
                (DM_IN_BUFFER | DM_OUT_BUFFER).0,
            )
        };
        if merged != IDOK {
            return Err("The printer does not accept that label size.".to_string());
        }
        Ok(storage)
    })();

    // SAFETY: `handle` was opened above.
    unsafe {
        let _ = ClosePrinter(handle);
    }
    result
}

/// Prints every page `job.copies` times as one document.
pub(super) fn print(job: &PrintJob, pages: &[Bitmap]) -> Result<(), String> {
    let name = HSTRING::from(job.printer.as_str());
    let devmode = label_devmode(&name, job.width_mm, job.height_mm)?;

    // SAFETY: `devmode` outlives the call; the strings are valid UTF-16.
    let hdc = unsafe {
        CreateDCW(
            PCWSTR::null(),
            &name,
            PCWSTR::null(),
            Some(devmode.as_ptr().cast::<DEVMODEW>()),
        )
    };
    if hdc.is_invalid() {
        return Err("Could not connect to the printer.".to_string());
    }
    let context = DeviceContext(hdc);

    let document_name = HSTRING::from("SimpleBash labels");
    let document = DOCINFOW {
        cbSize: size_of::<DOCINFOW>() as i32,
        lpszDocName: PCWSTR(document_name.as_ptr()),
        lpszOutput: PCWSTR::null(),
        lpszDatatype: PCWSTR::null(),
        fwType: 0,
    };
    // SAFETY: valid device context and document description.
    if unsafe { StartDocW(context.0, &document) } <= 0 {
        return Err("The printer would not start the labels.".to_string());
    }

    // SAFETY: valid device context for all the queries below.
    let (dpi_x, dpi_y, offset_x, offset_y) = unsafe {
        SetStretchBltMode(context.0, COLORONCOLOR);
        (
            GetDeviceCaps(Some(context.0), LOGPIXELSX),
            GetDeviceCaps(Some(context.0), LOGPIXELSY),
            GetDeviceCaps(Some(context.0), PHYSICALOFFSETX),
            GetDeviceCaps(Some(context.0), PHYSICALOFFSETY),
        )
    };
    let scale = |pixels: u32, device_dpi: i32| -> i32 {
        (f64::from(pixels) * f64::from(device_dpi) / f64::from(job.dpi)).round() as i32
    };

    for _ in 0..job.copies {
        for page in pages {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: page.width as i32,
                    // Negative height: the rows are stored top first.
                    biHeight: -(page.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                bmiColors: [RGBQUAD::default()],
            };
            // SAFETY: valid context; `bgra` holds `width * height * 4` bytes.
            let drawn = unsafe {
                if StartPage(context.0) <= 0 {
                    0
                } else {
                    let lines = StretchDIBits(
                        context.0,
                        // The image starts at the paper's corner, not the printable area's.
                        -offset_x,
                        -offset_y,
                        scale(page.width, dpi_x),
                        scale(page.height, dpi_y),
                        0,
                        0,
                        page.width as i32,
                        page.height as i32,
                        Some(page.bgra.as_ptr().cast::<c_void>()),
                        &info,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                    if EndPage(context.0) <= 0 {
                        0
                    } else {
                        lines
                    }
                }
            };
            if drawn <= 0 {
                // SAFETY: the document was started above.
                unsafe {
                    AbortDoc(context.0);
                }
                return Err("The printer stopped while printing the labels.".to_string());
            }
        }
    }
    // SAFETY: the document was started above.
    if unsafe { EndDoc(context.0) } <= 0 {
        return Err("The printer did not finish the labels.".to_string());
    }
    Ok(())
}
