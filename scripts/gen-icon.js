#!/usr/bin/env node
// Generates assets/icon.ico (Windows exe) and frontend/public/favicon.{ico,svg}.
// No npm dependencies: pure Node.js.

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

// ---------------------------------------------------------------------------
// CRC32
// ---------------------------------------------------------------------------
const CRC_TABLE = new Uint32Array(256);
for (let i = 0; i < 256; i++) {
  let c = i;
  for (let j = 0; j < 8; j++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  CRC_TABLE[i] = c;
}
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

// ---------------------------------------------------------------------------
// PNG encoder
// ---------------------------------------------------------------------------
function pngChunk(type, data) {
  const t = Buffer.from(type, 'ascii');
  const lenBuf = Buffer.alloc(4);
  lenBuf.writeUInt32BE(data.length);
  const crcInput = Buffer.concat([t, data]);
  const crcBuf = Buffer.alloc(4);
  crcBuf.writeUInt32BE(crc32(crcInput));
  return Buffer.concat([lenBuf, t, data, crcBuf]);
}

function encodePNG(size, pixelsRGBA) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; ihdr[9] = 6; // 8-bit RGBA

  // Raw scanlines: 1 filter byte + 4 bytes per pixel
  const stride = 1 + size * 4;
  const raw = Buffer.alloc(size * stride);
  for (let y = 0; y < size; y++) {
    raw[y * stride] = 0; // filter: None
    for (let x = 0; x < size; x++) {
      const s = (y * size + x) * 4;
      const d = y * stride + 1 + x * 4;
      raw[d]     = pixelsRGBA[s];
      raw[d + 1] = pixelsRGBA[s + 1];
      raw[d + 2] = pixelsRGBA[s + 2];
      raw[d + 3] = pixelsRGBA[s + 3];
    }
  }

  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

// ---------------------------------------------------------------------------
// Icon renderer
// Design in 100×100 coordinate space:
//   Background: rounded rect rx=18, color #1D4ED8
//   Arrow:      right-pointing notched arrow, color white
//               M14 40 L56 40 L56 23 L87 50 L56 77 L56 60 L14 60 Z
// ---------------------------------------------------------------------------
const ARROW_VERTS = [
  [14, 40], [56, 40], [56, 23], [87, 50], [56, 77], [56, 60], [14, 60],
];
const BG_RX = 18; // corner radius in 0-100 space

function inPolygon(px, py, verts) {
  let inside = false;
  for (let i = 0, j = verts.length - 1; i < verts.length; j = i++) {
    const [xi, yi] = verts[i];
    const [xj, yj] = verts[j];
    if ((yi > py) !== (yj > py) && px < ((xj - xi) * (py - yi)) / (yj - yi) + xi) {
      inside = !inside;
    }
  }
  return inside;
}

function inRoundedRect(px, py) {
  const dx = Math.max(0, BG_RX - px, px - (100 - BG_RX));
  const dy = Math.max(0, BG_RX - py, py - (100 - BG_RX));
  return dx * dx + dy * dy <= BG_RX * BG_RX;
}

function renderIcon(size) {
  // 4× supersampling for antialiasing
  const SS = 4;
  const big = size * SS;
  const buf = new Float32Array(big * big * 4); // RGBA [0..1]

  const scale = big / 100;

  // Background: #1D4ED8 = rgb(29, 78, 216)
  const [bgR, bgG, bgB] = [29 / 255, 78 / 255, 216 / 255];

  for (let py = 0; py < big; py++) {
    for (let px = 0; px < big; px++) {
      const cx = (px + 0.5) / scale;
      const cy = (py + 0.5) / scale;
      const i = (py * big + px) * 4;
      if (inPolygon(cx, cy, ARROW_VERTS)) {
        buf[i] = 1; buf[i + 1] = 1; buf[i + 2] = 1; buf[i + 3] = 1;
      } else if (inRoundedRect(cx, cy)) {
        buf[i] = bgR; buf[i + 1] = bgG; buf[i + 2] = bgB; buf[i + 3] = 1;
      }
    }
  }

  // Downsample
  const pixels = new Uint8Array(size * size * 4);
  const n = SS * SS;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const i = ((y * SS + sy) * big + (x * SS + sx)) * 4;
          r += buf[i]; g += buf[i + 1]; b += buf[i + 2]; a += buf[i + 3];
        }
      }
      const p = (y * size + x) * 4;
      pixels[p]     = Math.round((r / n) * 255);
      pixels[p + 1] = Math.round((g / n) * 255);
      pixels[p + 2] = Math.round((b / n) * 255);
      pixels[p + 3] = Math.round((a / n) * 255);
    }
  }
  return pixels;
}

// ---------------------------------------------------------------------------
// ICO packer (PNG-in-ICO, supported since Windows Vista)
// ---------------------------------------------------------------------------
function packICO(images) {
  const headerSize = 6 + images.length * 16;
  let offset = headerSize;
  const offsets = images.map(img => { const o = offset; offset += img.png.length; return o; });

  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0); // reserved
  header.writeUInt16LE(1, 2); // type: ICO
  header.writeUInt16LE(images.length, 4);

  const dirs = images.map((img, i) => {
    const d = Buffer.alloc(16);
    d[0] = img.size >= 256 ? 0 : img.size; // width  (0 = 256)
    d[1] = img.size >= 256 ? 0 : img.size; // height (0 = 256)
    d[2] = 0; d[3] = 0;                    // color count, reserved
    d.writeUInt16LE(1, 4);                  // planes
    d.writeUInt16LE(32, 6);                 // bits per pixel
    d.writeUInt32LE(img.png.length, 8);
    d.writeUInt32LE(offsets[i], 12);
    return d;
  });

  return Buffer.concat([header, ...dirs, ...images.map(img => img.png)]);
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
const root = path.resolve(__dirname, '..');

const ICO_SIZES = [16, 32, 48, 256];
const images = ICO_SIZES.map(size => ({
  size,
  png: encodePNG(size, renderIcon(size)),
}));

const assetsDir = path.join(root, 'assets');
const publicDir = path.join(root, 'frontend', 'public');
fs.mkdirSync(assetsDir, { recursive: true });
fs.mkdirSync(publicDir, { recursive: true });

// Executable icon
fs.writeFileSync(path.join(assetsDir, 'icon.ico'), packICO(images));
console.log('✓  assets/icon.ico');

// Favicon ICO (skip 256 - overkill for browsers)
const faviconImages = images.filter(img => img.size <= 48);
fs.writeFileSync(path.join(publicDir, 'favicon.ico'), packICO(faviconImages));
console.log('✓  frontend/public/favicon.ico');

// Favicon SVG (vector, crisp at any size, preferred by modern browsers)
const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <rect width="100" height="100" rx="18" fill="#1D4ED8"/>
  <path d="M14 40 L56 40 L56 23 L87 50 L56 77 L56 60 L14 60 Z" fill="white"/>
</svg>
`;
fs.writeFileSync(path.join(publicDir, 'favicon.svg'), svg);
console.log('✓  frontend/public/favicon.svg');
