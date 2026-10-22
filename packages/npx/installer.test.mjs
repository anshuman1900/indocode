import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, randomBytes, sign } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { PUBKEY, assetFor, desktopEntry, verifyMinisign } from './installer.mjs';

const b64 = b => Buffer.from(b).toString('base64');

function fixture(alg = 'ED', keyId = randomBytes(8)) {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const pk = Buffer.from(publicKey.export({ format: 'jwk' }).x, 'base64url');
  const pubkey = b64(`untrusted comment: minisign public key\n${b64(Buffer.concat([Buffer.from('Ed'), keyId, pk]))}\n`);
  const data = randomBytes(1000);
  const raw = sign(null, alg === 'ED' ? createHash('blake2b512').update(data).digest() : data, privateKey);
  const comment = 'timestamp:1 file:x';
  const global = sign(null, Buffer.concat([raw, Buffer.from(comment)]), privateKey);
  const sigFile = b64(`untrusted comment: signature\n${b64(Buffer.concat([Buffer.from(alg), keyId, raw]))}\ntrusted comment: ${comment}\n${b64(global)}\n`);
  return { pubkey, data, sigFile };
}

for (const alg of ['ED', 'Ed']) {
  test(`verifies a ${alg} signature`, () => {
    const { pubkey, data, sigFile } = fixture(alg);
    verifyMinisign(data, sigFile, pubkey);
  });
}

test('rejects a tampered byte', () => {
  const { pubkey, data, sigFile } = fixture();
  data[5] ^= 1;
  assert.throws(() => verifyMinisign(data, sigFile, pubkey), /does not match/);
});

test('rejects a different key id', () => {
  const a = fixture();
  const b = fixture();
  assert.throws(() => verifyMinisign(a.data, a.sigFile, b.pubkey), /different key/);
});

test('rejects a tampered trusted comment', () => {
  const { pubkey, data, sigFile } = fixture();
  const text = Buffer.from(sigFile, 'base64').toString().replace('timestamp:1', 'timestamp:2');
  assert.throws(() => verifyMinisign(data, b64(text), pubkey), /comment/);
});

test('picks the asset per platform', () => {
  assert.equal(assetFor('darwin', 'arm64'), 'Indocode-macOS-arm64.app.tar.gz');
  assert.equal(assetFor('win32', 'x64'), 'Indocode-Windows-x64-setup.exe');
  assert.equal(assetFor('linux', 'x64'), 'Indocode-Linux-x86_64.AppImage');
  assert.equal(assetFor('darwin', 'x64'), 'Indocode-macOS-x64.app.tar.gz');
  assert.throws(() => assetFor('darwin', 'ia32'), /no build/);
  assert.equal(assetFor('linux', 'arm64'), 'Indocode-Linux-aarch64.AppImage');
  assert.throws(() => assetFor('win32', 'arm64'), /no build/);
});

test('writes a desktop entry', () => {
  const text = desktopEntry('/h/.local/bin/indocode', '/h/icon.png');
  assert.match(text, /^\[Desktop Entry\]\n/);
  assert.match(text, /\nExec=\/h\/\.local\/bin\/indocode\n/);
  assert.match(text, /\nIcon=\/h\/icon\.png\n/);
});

test('embedded pubkey matches tauri.conf.json', () => {
  const conf = JSON.parse(readFileSync(new URL('../../src-tauri/tauri.conf.json', import.meta.url)));
  assert.equal(PUBKEY, conf.plugins.updater.pubkey);
});
