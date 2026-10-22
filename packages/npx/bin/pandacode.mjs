#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { install } from '../installer.mjs';

const args = process.argv.slice(2);
if (args.includes('--help') || args.includes('-h')) {
  console.log(`Usage: npx indocode [--dry-run] [--help] [--version]

Downloads the latest Indocode release from GitHub, verifies its signature and
installs it (macOS Apple silicon, Windows x64, Linux x64).

  --dry-run   download and verify only
  --version   print this installer's version`);
} else if (args.includes('--version') || args.includes('-v')) {
  console.log(JSON.parse(readFileSync(new URL('../package.json', import.meta.url))).version);
} else {
  install({ dryRun: args.includes('--dry-run') }).catch(error => {
    console.error(`indocode: ${error.message}`);
    process.exit(1);
  });
}
