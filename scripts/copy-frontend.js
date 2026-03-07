#!/usr/bin/env node
// Copy frontend from src to dist so dist is generated from src before dev/build.
// Single source of truth: lilypad/index.html, lilypad/src/main.js, lilypad/src/main.css

const fs = require('fs');
const path = require('path');

const root = path.join(__dirname, '..');
const distDir = path.join(root, 'dist');
const distSrcDir = path.join(distDir, 'src');

if (!fs.existsSync(distSrcDir)) {
  fs.mkdirSync(distSrcDir, { recursive: true });
}

fs.copyFileSync(path.join(root, 'index.html'), path.join(distDir, 'index.html'));
fs.copyFileSync(path.join(root, 'src', 'main.js'), path.join(distSrcDir, 'main.js'));
fs.copyFileSync(path.join(root, 'src', 'main.css'), path.join(distSrcDir, 'main.css'));

console.log('Frontend copied: index.html, src/main.js, src/main.css -> dist/');
