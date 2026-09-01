#!/usr/bin/env node

'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const { spawnSync } = require('child_process');

const pkg = require('../package.json');

const REPO = 'tunardev/nightjar';

/**
 * @returns {string} one of the five Rust target triples release.yml builds
 */
function resolveTarget() {
  const { platform, arch } = process;
  if (platform === 'darwin' && arch === 'arm64') return 'aarch64-apple-darwin';
  if (platform === 'darwin' && arch === 'x64') return 'x86_64-apple-darwin';
  if (platform === 'linux' && arch === 'arm64') return 'aarch64-unknown-linux-gnu';
  if (platform === 'linux' && arch === 'x64') {
    return isMusl() ? 'x86_64-unknown-linux-musl' : 'x86_64-unknown-linux-gnu';
  }
  throw new Error(
    `nightjar has no prebuilt binary for ${platform}/${arch}. ` +
      'Supported combinations: darwin/arm64, darwin/x64, linux/arm64, linux/x64.'
  );
}

/**
 * @returns {boolean}
 */
function isMusl() {
  // glibc always populates this field; its absence on Linux is the same musl
  // tell used by other prebuilt-binary installers (e.g. esbuild), and avoids
  // depending on `ldd` being on PATH.
  try {
    const report = process.report && process.report.getReport();
    if (report && report.header && report.header.glibcVersionRuntime) {
      return false;
    }
  } catch {
    // process.report can be disabled; fall through to the filesystem check.
  }
  try {
    return fs.existsSync('/lib/ld-musl-x86_64.so.1');
  } catch {
    return false;
  }
}

/**
 * @param {string} target
 * @returns {string}
 */
function cacheDirFor(target) {
  return path.join(__dirname, '..', '.cache', pkg.version, target);
}

/**
 * @param {string} url
 * @param {string} destPath
 * @param {number} [redirectsLeft]
 * @returns {Promise<void>}
 */
function download(url, destPath, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(destPath);
    https
      .get(url, { headers: { 'user-agent': 'nightjar-npm-wrapper' } }, (res) => {
        const { statusCode, headers } = res;
        if (statusCode >= 300 && statusCode < 400 && headers.location) {
          file.close();
          fs.unlink(destPath, () => {});
          if (redirectsLeft <= 0) {
            reject(new Error(`too many redirects fetching ${url}`));
            return;
          }
          if (!headers.location.startsWith('https://')) {
            reject(new Error(`refusing a redirect to a non-https location: ${headers.location}`));
            return;
          }
          resolve(download(headers.location, destPath, redirectsLeft - 1));
          return;
        }
        if (statusCode !== 200) {
          file.close();
          fs.unlink(destPath, () => {});
          res.resume();
          reject(new Error(`GET ${url} returned HTTP ${statusCode}`));
          return;
        }
        res.pipe(file);
        file.on('finish', () => file.close(() => resolve()));
        file.on('error', reject);
      })
      .on('error', (err) => {
        file.close();
        fs.unlink(destPath, () => {});
        reject(err);
      });
  });
}

/**
 * Fetches a small text file (following redirects the same way `download`
 * does), or returns null on any non-200 status — a missing checksum file
 * means "this release predates checksums", not an error.
 * @param {string} url
 * @param {number} [redirectsLeft]
 * @returns {Promise<string | null>}
 */
function fetchText(url, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { 'user-agent': 'nightjar-npm-wrapper' } }, (res) => {
        const { statusCode, headers } = res;
        if (statusCode >= 300 && statusCode < 400 && headers.location) {
          if (redirectsLeft <= 0 || !headers.location.startsWith('https://')) {
            resolve(null);
            return;
          }
          resolve(fetchText(headers.location, redirectsLeft - 1));
          return;
        }
        if (statusCode !== 200) {
          res.resume();
          resolve(null);
          return;
        }
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (chunk) => (body += chunk));
        res.on('end', () => resolve(body));
        res.on('error', reject);
      })
      .on('error', reject);
  });
}

/**
 * @param {string} filePath
 * @returns {Promise<string>} lowercase hex sha256 digest
 */
function sha256File(filePath) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash('sha256');
    const stream = fs.createReadStream(filePath);
    stream.on('data', (chunk) => hash.update(chunk));
    stream.on('end', () => resolve(hash.digest('hex')));
    stream.on('error', reject);
  });
}

/**
 * Resolves the cached binary path for the current platform, downloading and
 * extracting the matching release archive first if it isn't already cached.
 * @returns {Promise<string>}
 */
async function ensureBinary() {
  const target = resolveTarget();
  const dir = cacheDirFor(target);
  const binPath = path.join(dir, 'nightjar');
  if (fs.existsSync(binPath)) {
    return binPath;
  }

  fs.mkdirSync(dir, { recursive: true });
  const url = `https://github.com/${REPO}/releases/download/v${pkg.version}/nightjar-${target}.tar.gz`;
  const archivePath = path.join(dir, 'nightjar.tar.gz');

  try {
    await download(url, archivePath);
  } catch (err) {
    throw new Error(
      `could not download the nightjar binary from ${url}: ${err.message}. ` +
        `This usually means release v${pkg.version} doesn't exist on GitHub yet, ` +
        'or the network is unreachable.'
    );
  }

  // Absent on any release built before this file existed; verify when
  // present, skip rather than fail when it's missing.
  const checksums = await fetchText(`${url}.sha256`);
  if (checksums) {
    const expected = checksums.trim().split(/\s+/)[0];
    const actual = await sha256File(archivePath);
    if (expected !== actual) {
      fs.unlinkSync(archivePath);
      throw new Error(`checksum mismatch for ${url}: expected ${expected}, got ${actual}`);
    }
  }

  const extract = spawnSync('tar', ['-xzf', archivePath, '-C', dir], { stdio: 'inherit' });
  if (extract.status !== 0) {
    throw new Error(`failed to extract ${archivePath} (tar exited with status ${extract.status})`);
  }
  fs.unlinkSync(archivePath);

  if (!fs.existsSync(binPath)) {
    throw new Error(`extracted archive did not contain the expected 'nightjar' binary at ${binPath}`);
  }
  // bsdtar/GNU tar don't reliably preserve the executable bit across
  // platforms, so set it explicitly rather than trust the archive.
  fs.chmodSync(binPath, 0o755);
  return binPath;
}

async function main() {
  const args = process.argv.slice(2);
  let binPath = process.env.NIGHTJAR_BIN;

  if (!binPath) {
    try {
      binPath = await ensureBinary();
    } catch (err) {
      process.stderr.write(`nightjar: ${err.message}\n`);
      process.exit(1);
    }
  }

  const result = spawnSync(binPath, args, { stdio: 'inherit' });

  if (result.error) {
    process.stderr.write(`nightjar: failed to run ${binPath}: ${result.error.message}\n`);
    process.exit(1);
  }
  if (result.signal) {
    process.stderr.write(`nightjar: terminated by signal ${result.signal}\n`);
    // 128+signal is the shell convention (e.g. 130 for SIGINT/Ctrl-C), so a
    // caller checking the exit code sees the same value a native binary
    // killed the same way would produce.
    const signalNumber = os.constants.signals[result.signal];
    process.exit(signalNumber ? 128 + signalNumber : 1);
  }
  process.exit(result.status === null ? 1 : result.status);
}

main();
