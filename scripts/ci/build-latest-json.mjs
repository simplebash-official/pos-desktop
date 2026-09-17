// Assemble the Tauri v2 updater manifest (`latest.json`) from the per-target
// bundle folders downloaded in the workflow's `publish` job.
//
//   node scripts/ci/build-latest-json.mjs \
//     --artifacts artifacts --version 0.2.0 --tag v0.2.0 \
//     --repo simplebash-official/releases --notes "$(cat NOTES.md)" \
//     --out latest.json
//
// Layout it expects (one folder per matrix target, produced by upload-artifact
// with name `bundle-<triple>`):
//
//   artifacts/bundle-aarch64-apple-darwin/**/*.app.tar.gz(+.sig)
//   artifacts/bundle-x86_64-apple-darwin/**/*.app.tar.gz(+.sig)
//   artifacts/bundle-x86_64-unknown-linux-gnu/**/*.AppImage(+.sig)
//   artifacts/bundle-x86_64-pc-windows-msvc/**/*-setup.exe(+.sig)
//
// It renames every release asset to a space-free name in place (GitHub would
// rewrite spaces to dots on upload anyway) so the URLs here match exactly, and
// prints the flat list of files to upload on stdout (newline-separated).

import { readdirSync, statSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { join, dirname, basename } from 'node:path';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, cur, i, arr) => {
    if (cur.startsWith('--')) acc.push([cur.slice(2), arr[i + 1]]);
    return acc;
  }, [])
);

for (const required of ['artifacts', 'version', 'tag', 'repo', 'out']) {
  if (!args[required]) {
    console.error(`missing --${required}`);
    process.exit(1);
  }
}

// matrix target triple -> Tauri updater platform key + the updater artifact glob
const TARGETS = [
  { triple: 'aarch64-apple-darwin', key: 'darwin-aarch64', ext: '.app.tar.gz' },
  { triple: 'x86_64-unknown-linux-gnu', key: 'linux-x86_64', ext: '.AppImage' },
  { triple: 'x86_64-pc-windows-msvc', key: 'windows-x86_64', ext: '-setup.exe' },
];

const walk = (dir) => {
  const out = [];
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return out;
  }
  for (const name of entries) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) out.push(...walk(full));
    else out.push(full);
  }
  return out;
};

const spaceFree = (p) => join(dirname(p), basename(p).replace(/\s+/g, '.'));

const platforms = {};
const uploads = new Set();

for (const { triple, key, ext } of TARGETS) {
  const root = join(args.artifacts, `bundle-${triple}`);
  const files = walk(root);

  // The updater artifact for this target and its detached signature.
  const foundArtifact = files.find((f) => f.endsWith(ext) && !f.endsWith(`${ext}.sig`));
  const foundSig = files.find((f) => f.endsWith(`${ext}.sig`));
  if (!foundArtifact || !foundSig) {
    console.error(`::error::no ${ext} (+.sig) found under ${root}`);
    process.exit(1);
  }

  // Give the updater artifact a fully deterministic, per-target name — the two
  // macOS `.app.tar.gz` blobs are otherwise identical and would collide as
  // release assets. Installers (dmg/exe/AppImage/deb) already carry version +
  // arch, so those only get spaces stripped.
  const renameTo = (p, target) => {
    if (p !== target) renameSync(p, target);
    return target;
  };
  const updaterName = `simplebash-pos_${key}${ext}`;
  const artifact = renameTo(foundArtifact, join(dirname(foundArtifact), updaterName));
  const sig = renameTo(foundSig, join(dirname(foundSig), `${updaterName}.sig`));

  const assetName = basename(artifact);
  platforms[key] = {
    signature: readFileSync(sig, 'utf-8').trim(),
    url: `https://github.com/${args.repo}/releases/download/${args.tag}/${encodeURIComponent(assetName)}`,
  };
  uploads.add(artifact);

  // Also ship the user-facing installer(s) for this target (dmg / deb / nsis
  // exe) so the release page has something to click, not only the updater
  // blob. Re-walk after the renames above so paths are current.
  for (const f of walk(root)) {
    if (f === artifact || f === sig) continue;
    if (!/\.(dmg|deb|exe|msi)$/.test(f)) continue;
    const renamed = spaceFree(f);
    if (renamed !== f) renameSync(f, renamed);
    uploads.add(renamed);
  }
}

const manifest = {
  version: args.version,
  notes: args.notes || `SimpleBash POS ${args.tag}`,
  pub_date: new Date().toISOString(),
  platforms,
};

writeFileSync(args.out, JSON.stringify(manifest, null, 2) + '\n');
uploads.add(args.out);

process.stdout.write([...uploads].join('\n') + '\n');
