import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { resolve } from 'node:path';

export const root = fileURLToPath(new URL('../../', import.meta.url));

// One entry per published desktop build. `asset` is the stable file name on a
// GitHub Release, so https://github.com/aicodeking/pandacode/releases/latest/download/<asset>
// always points at the newest build for that platform. `updater` is the signed file the
// in-app updater installs (relative to the bundle directory), published as `updaterAsset`.
export const targets = Object.freeze({
  'darwin-aarch64': { platform: 'darwin', arch: 'arm64', triple: 'aarch64-apple-darwin', bundles: 'app,dmg', bundleDir: 'dmg', built: v => `Indocode_${v}_aarch64.dmg`, asset: 'Indocode-macOS-arm64.dmg', updater: () => 'macos/Indocode.app.tar.gz', updaterAsset: 'Indocode-macOS-arm64.app.tar.gz' },
  'darwin-x86_64': { platform: 'darwin', arch: 'x64', triple: 'x86_64-apple-darwin', bundles: 'app,dmg', bundleDir: 'dmg', built: v => `Indocode_${v}_x64.dmg`, asset: 'Indocode-macOS-x64.dmg', updater: () => 'macos/Indocode.app.tar.gz', updaterAsset: 'Indocode-macOS-x64.app.tar.gz' },
  'windows-x86_64': { platform: 'win32', arch: 'x64', triple: 'x86_64-pc-windows-msvc', bundles: 'nsis', bundleDir: 'nsis', built: v => `Indocode_${v}_x64-setup.exe`, asset: 'Indocode-Windows-x64-setup.exe', updater: v => `nsis/Indocode_${v}_x64-setup.exe`, updaterAsset: 'Indocode-Windows-x64-setup.exe' },
  'linux-x86_64': { platform: 'linux', arch: 'x64', triple: 'x86_64-unknown-linux-gnu', bundles: 'appimage', bundleDir: 'appimage', built: v => `Indocode_${v}_amd64.AppImage`, asset: 'Indocode-Linux-x86_64.AppImage', updater: v => `appimage/Indocode_${v}_amd64.AppImage`, updaterAsset: 'Indocode-Linux-x86_64.AppImage' },
  'linux-aarch64': { platform: 'linux', arch: 'arm64', triple: 'aarch64-unknown-linux-gnu', bundles: 'appimage', bundleDir: 'appimage', built: v => `Indocode_${v}_aarch64.AppImage`, asset: 'Indocode-Linux-aarch64.AppImage', updater: v => `appimage/Indocode_${v}_aarch64.AppImage`, updaterAsset: 'Indocode-Linux-aarch64.AppImage' },
});

export function targetConfig(value) {
  if (typeof value !== 'string' || !Object.hasOwn(targets, value)) throw new Error(`Unsupported release target. Choose one of: ${Object.keys(targets).join(', ')}.`);
  return targets[value];
}

export function hostTarget(platform = process.platform, arch = process.arch) {
  const match = Object.entries(targets).find(([, config]) => config.platform === platform && config.arch === arch);
  if (!match) throw new Error(`No release target for ${platform}/${arch}.`);
  return match[0];
}

export function appImageBundleOptions(releaseTarget) {
  targetConfig(releaseTarget);
  // Tauri maps AppImage destination paths to build-host source paths. Keep the
  // link-opening helper inside the package without adding a desktop plugin.
  return releaseTarget.startsWith('linux-')
    ? { linux: { appimage: { files: { '/usr/bin/xdg-open': '/usr/bin/xdg-open' } } } }
    : {};
}

export function required(name, env = process.env) {
  const value = env[name]?.trim();
  if (!value) throw new Error(`Missing ${name}. Set it in your shell or CI environment.`);
  return value;
}

export function version(value) {
  if (typeof value !== 'string' || value.length > 32 || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value) || value.split('.').some(part => !Number.isSafeInteger(Number(part)))) throw new Error('Release version must be a stable SemVer such as 0.3.0.');
  return value;
}

export async function json(path) { return JSON.parse(await readFile(path, 'utf8')); }

/** Cargo.lock text with the `indocode` package at `next`. Keeps the file's line endings,
 * so a Windows checkout with CRLF is updated too, and fails instead of leaving it unchanged. */
export function cargoLockVersion(lock, next) {
  const entry = /(\[\[package\]\]\r?\nname = "indocode"\r?\nversion = )"[^"]+"/;
  if (!entry.test(lock)) throw new Error('No indocode package in src-tauri/Cargo.lock.');
  return lock.replace(entry, `$1"${version(next)}"`);
}

export async function digest(path) {
  const sha = createHash('sha256');
  for await (const chunk of createReadStream(path)) sha.update(chunk);
  return sha.digest('hex');
}

export function run(command, args, options = {}) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(command, args, { cwd: root, stdio: 'inherit', ...options });
    child.on('error', () => reject(new Error(`Could not start ${command}.`)));
    child.on('exit', (code) => code === 0 ? resolvePromise() : reject(new Error(`${command} failed (exit ${code}).`)));
  });
}

export function releaseDirectory(releaseVersion) {
  return resolve(root, 'release', version(releaseVersion));
}

/** The release's updater manifest for the in-app updater: each platform's signed file and
 * its signature. `signatures` maps release targets to the contents of their `.sig` files.
 * Files are on the repository that publishes the release (GITHUB_REPOSITORY in Actions),
 * so a fork's release points at its own assets. */
export function updaterManifest(releaseVersion, notes, signatures, pubDate = new Date().toISOString(), repository = process.env.GITHUB_REPOSITORY || 'aicodeking/pandacode') {
  const v = version(releaseVersion);
  if (!/^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/.test(repository)) throw new Error(`Not a GitHub repository: ${repository}`);
  const platforms = {};
  for (const [target, signature] of Object.entries(signatures)) {
    const config = targetConfig(target);
    if (typeof signature !== 'string' || !signature.trim()) throw new Error(`Missing updater signature for ${target}.`);
    platforms[target] = { signature: signature.trim(), url: `https://github.com/${repository}/releases/download/v${v}/${config.updaterAsset}` };
  }
  if (!Object.keys(platforms).length) throw new Error('No signed updater files to publish.');
  return { version: v, notes: String(notes ?? '').trim(), pub_date: pubDate, platforms };
}
