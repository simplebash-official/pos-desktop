// Packs the assembled Typst templates dir into one AES-256 zip
// (`src-tauri/resources/templates.pack`) so the installer doesn't carry the
// designs as plain files. The shell (`orchestrator::sync_render_assets`)
// unpacks it with the same key. Obfuscation only: the key ships in the app.
//
//   pack-templates <templates-dir> <out.pack>
//
// Key: $TEMPLATES_PACK_KEY, or the public dev key (forks, local builds).
// Must match DEV_TEMPLATES_PACK_KEY in src-tauri/src/orchestrator.rs.

use std::fs::{self, File};
use std::io;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, ZipWriter};

const DEV_KEY: &str = "simplebash-dev-templates";

fn add_dir(
    zip: &mut ZipWriter<File>,
    root: &Path,
    dir: &Path,
    opts: SimpleFileOptions,
    key: &str,
) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            add_dir(zip, root, &path, opts, key)?;
            continue;
        }
        let rel = path.strip_prefix(root).unwrap();
        let name = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        zip.start_file(name, opts.with_aes_encryption(AesMode::Aes256, key))?;
        io::copy(&mut File::open(&path)?, zip)?;
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, src, out] = args.as_slice() else {
        eprintln!("usage: pack-templates <templates-dir> <out.pack>");
        std::process::exit(2);
    };
    let key = std::env::var("TEMPLATES_PACK_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| DEV_KEY.to_string());
    let mut zip = ZipWriter::new(File::create(out)?);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    add_dir(&mut zip, Path::new(src), Path::new(src), opts, &key)?;
    zip.finish()?;
    Ok(())
}
