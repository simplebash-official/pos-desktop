//! Native printing bridge for desktop platforms.
//!
//! On macOS, WebKit (WKWebView) silently ignores `iframe.contentWindow.print()`,
//! leaving web-based iframe printing broken. This module bridges PDF printing to
//! Apple's native `PDFKit` framework via `NSPrintOperation`, presenting the standard
//! macOS print dialog sheet directly with high-fidelity vector PDF data.

#[tauri::command]
pub async fn print_pdf_native(
    app: tauri::AppHandle,
    pdf_base64: Option<String>,
    pdf_url: Option<String>,
    pdf_bytes: Option<Vec<u8>>,
    title: Option<String>,
) -> Result<(), String> {
    let call = crate::logging::CommandLog::start(
        "print_pdf_native",
        serde_json::json!({
            "pdf_url": pdf_url,
            "pdf_bytes_len": pdf_bytes.as_ref().map(Vec::len),
            "pdf_base64_len": pdf_base64.as_ref().map(String::len),
            "title": title,
        }),
    );
    call.finish(print_pdf_inner(app, pdf_base64, pdf_url, pdf_bytes, title).await)
}

async fn print_pdf_inner(
    app: tauri::AppHandle,
    pdf_base64: Option<String>,
    pdf_url: Option<String>,
    pdf_bytes: Option<Vec<u8>>,
    title: Option<String>,
) -> Result<(), String> {
    let bytes = if let Some(url) = pdf_url {
        let resp = reqwest::get(&url)
            .await
            .map_err(|e| format!("Failed to fetch PDF from {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "Document server returned status {}: {url}",
                resp.status()
            ));
        }
        resp.bytes()
            .await
            .map_err(|e| format!("Failed to read PDF bytes from {url}: {e}"))?
            .to_vec()
    } else if let Some(bytes) = pdf_bytes {
        bytes
    } else if let Some(b64) = pdf_base64 {
        use base64::prelude::*;
        BASE64_STANDARD
            .decode(b64.trim())
            .map_err(|e| format!("Failed to decode base64 PDF: {e}"))?
    } else {
        return Err(
            "No PDF payload provided (expected pdf_url, pdf_bytes, or pdf_base64)".to_string(),
        );
    };

    #[cfg(target_os = "macos")]
    {
        macos::print_pdf_bytes(app, bytes, title).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, bytes, title);
        Err("Native PDF printing is currently only implemented on macOS".to_string())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::autoreleasepool;
    use objc2::AnyThread;
    use objc2_app_kit::NSPrintInfo;
    use objc2_foundation::{MainThreadMarker, NSString, NSURL};
    use objc2_pdf_kit::{PDFDocument, PDFPrintScalingMode};
    use std::fs;
    use std::sync::mpsc;
    use tauri::AppHandle;

    pub async fn print_pdf_bytes(
        app: AppHandle,
        pdf_bytes: Vec<u8>,
        title: Option<String>,
    ) -> Result<(), String> {
        let temp_dir = std::env::temp_dir();
        let file_name = format!(
            "jana2u_print_{}_{}.pdf",
            std::process::id(),
            rand::random::<u32>()
        );
        let temp_path = temp_dir.join(&file_name);
        fs::write(&temp_path, &pdf_bytes)
            .map_err(|e| format!("Failed to write temporary PDF: {e}"))?;

        let (sender, receiver) = mpsc::channel();
        let temp_path_clone = temp_path.clone();

        app.run_on_main_thread(move || {
            let result = autoreleasepool(|_| {
                let mtm = MainThreadMarker::new()
                    .ok_or_else(|| "macOS print must run on the main thread".to_string())?;

                let path_str = temp_path_clone.to_string_lossy();
                let ns_path = NSString::from_str(&path_str);
                let file_url = NSURL::fileURLWithPath(&ns_path);

                let document = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &file_url) }
                    .ok_or_else(|| format!("Failed to load PDF in PDFKit: {path_str}"))?;

                let print_info = NSPrintInfo::sharedPrintInfo();
                let print_operation = unsafe {
                    document.printOperationForPrintInfo_scalingMode_autoRotate(
                        Some(&print_info),
                        PDFPrintScalingMode::PageScaleDownToFit,
                        true,
                        mtm,
                    )
                }
                .ok_or_else(|| "PDFKit could not create print operation".to_string())?;

                if let Some(ref t) = title {
                    let ns_title = NSString::from_str(t);
                    print_operation.setJobTitle(Some(&ns_title));
                }

                print_operation.setShowsPrintPanel(true);
                print_operation.setShowsProgressPanel(true);
                let _ = print_operation.runOperation();
                Ok(())
            });

            let _ = fs::remove_file(&temp_path_clone);
            let _ = sender.send(result);
        })
        .map_err(|e| {
            let _ = fs::remove_file(&temp_path);
            format!("Failed to dispatch to main thread: {e}")
        })?;

        receiver
            .recv()
            .map_err(|e| format!("Channel receive error: {e}"))?
    }
}
