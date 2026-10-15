//! macOS and Linux: list printers with `lpstat` and print with `lp` (CUPS). The
//! command lines are built as argument lists (never a shell string), so a printer
//! name can contain anything without being interpreted.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use super::PrintJob;

/// Absolute paths: a GUI app's PATH is minimal, and CUPS lives here on both systems.
const LPSTAT: &str = "/usr/bin/lpstat";
const LP: &str = "/usr/bin/lp";

/// Printer names from `lpstat -e` output (one destination per line).
pub(super) fn parse_destinations(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Millimetres for a CUPS media name: one decimal, no trailing ".0".
fn mm(value: f64) -> String {
    let text = format!("{:.1}", (value * 10.0).round() / 10.0);
    text.strip_suffix(".0").map_or(text.clone(), str::to_string)
}

/// The `lp` arguments for one job: a custom media of exactly the label size, the
/// image at 100% and at the resolution it was drawn at, so one image pixel is one
/// printer dot. Files come after `--` so none can be read as an option.
pub(super) fn lp_args(job: &PrintJob, files: &[PathBuf]) -> Vec<String> {
    let mut args = vec![
        "-d".to_string(),
        job.printer.clone(),
        "-n".to_string(),
        job.copies.to_string(),
        "-t".to_string(),
        "SimpleBash labels".to_string(),
        "-o".to_string(),
        format!("media=Custom.{}x{}mm", mm(job.width_mm), mm(job.height_mm)),
        "-o".to_string(),
        "scaling=100".to_string(),
        "-o".to_string(),
        format!("ppi={}", job.dpi),
        "--".to_string(),
    ];
    args.extend(files.iter().map(|file| file.to_string_lossy().into_owned()));
    args
}

pub(super) fn list_printers() -> Result<Vec<String>, String> {
    let output = Command::new(LPSTAT)
        .arg("-e")
        .output()
        .map_err(|e| missing_cups(&e))?;
    // `lpstat -e` exits non-zero with no output when no printer is set up at all.
    if !output.status.success() && output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    Ok(parse_destinations(&String::from_utf8_lossy(&output.stdout)))
}

fn missing_cups(error: &std::io::Error) -> String {
    format!("Printing is not available on this computer ({error}).")
}

/// A scratch folder for the page files, removed when dropped (also after an error).
struct PageFolder(PathBuf);

impl PageFolder {
    fn create() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path =
            std::env::temp_dir().join(format!("simplebash-labels-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).map_err(|e| format!("Could not prepare the labels: {e}"))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PageFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn print(job: &PrintJob) -> Result<(), String> {
    let folder = PageFolder::create()?;
    let mut files = Vec::with_capacity(job.pages.len());
    for (index, page) in job.pages.iter().enumerate() {
        let file = folder.path().join(format!("label-{:04}.png", index + 1));
        fs::write(&file, page).map_err(|e| format!("Could not prepare the labels: {e}"))?;
        files.push(file);
    }

    let output = Command::new(LP)
        .args(lp_args(job, &files))
        .output()
        .map_err(|e| missing_cups(&e))?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr);
    let reason = reason.trim();
    Err(if reason.is_empty() {
        "The printer did not accept the labels.".to_string()
    } else {
        format!("The printer did not accept the labels: {reason}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(printer: &str, w: f64, h: f64) -> PrintJob {
        PrintJob {
            printer: printer.into(),
            pages: vec![],
            width_mm: w,
            height_mm: h,
            dpi: 203,
            copies: 2,
        }
    }

    #[test]
    fn lpstat_output_becomes_a_clean_printer_list() {
        let out = "Zebra_ZD220\n\n  Brother QL-820  \nOffice_Laser\n";
        assert_eq!(
            parse_destinations(out),
            ["Zebra_ZD220", "Brother QL-820", "Office_Laser"]
        );
        assert!(parse_destinations("").is_empty());
        assert!(parse_destinations("\n  \n").is_empty());
    }

    #[test]
    fn media_sizes_are_written_the_way_cups_reads_them() {
        assert_eq!(mm(50.0), "50");
        assert_eq!(mm(49.999), "50");
        assert_eq!(mm(29.96), "30");
        assert_eq!(mm(58.5), "58.5");
        assert_eq!(mm(40.04), "40");
    }

    #[test]
    fn the_lp_command_line_names_the_printer_size_resolution_and_files() {
        let files = [
            PathBuf::from("/tmp/a/label-0001.png"),
            PathBuf::from("/tmp/a/label-0002.png"),
        ];
        let args = lp_args(&job("Zebra ZD220", 50.0, 30.0), &files);
        assert_eq!(
            args,
            [
                "-d",
                "Zebra ZD220",
                "-n",
                "2",
                "-t",
                "SimpleBash labels",
                "-o",
                "media=Custom.50x30mm",
                "-o",
                "scaling=100",
                "-o",
                "ppi=203",
                "--",
                "/tmp/a/label-0001.png",
                "/tmp/a/label-0002.png",
            ]
        );
    }

    #[test]
    fn a_printer_name_that_looks_like_an_option_stays_a_value() {
        let args = lp_args(
            &job("-o evil=1", 50.0, 30.0),
            &[PathBuf::from("/tmp/x.png")],
        );
        let at = args.iter().position(|a| a == "-d").unwrap();
        assert_eq!(args[at + 1], "-o evil=1");
        // Files can never be taken for options either.
        assert_eq!(args[args.len() - 2], "--");
    }
}
