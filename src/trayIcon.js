'use strict';

const zlib = require('node:zlib');
const { nativeImage } = require('electron');

// Tray icons are drawn here rather than shipped as binary assets, so the repo
// stays source-only and the icon can encode status by colour on both platforms.

const CRC = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buf) {
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = CRC[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function encodePng(width, height, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // colour type: RGBA
  // 10-12: compression, filter, interlace — all 0

  // One filter byte (0 = none) per scanline, then the row's pixels.
  const raw = Buffer.alloc(height * (1 + width * 4));
  for (let y = 0; y < height; y++) {
    const src = y * width * 4;
    const dst = y * (1 + width * 4);
    raw[dst] = 0;
    rgba.copy(raw, dst + 1, src, src + width * 4);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// A ring — reads as a port or tunnel mouth, and stays legible at 16px.
// Supersampled 3x3 per pixel so the curve is not jagged.
function ring(size, [r, g, b]) {
  const rgba = Buffer.alloc(size * size * 4);
  const c = (size - 1) / 2;
  const outer = size * 0.46;
  const inner = size * 0.2;
  const S = 3;

  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let hits = 0;
      for (let sy = 0; sy < S; sy++) {
        for (let sx = 0; sx < S; sx++) {
          const px = x + (sx + 0.5) / S - 0.5;
          const py = y + (sy + 0.5) / S - 0.5;
          const d = Math.hypot(px - c, py - c);
          if (d <= outer && d >= inner) hits++;
        }
      }
      const i = (y * size + x) * 4;
      rgba[i] = r;
      rgba[i + 1] = g;
      rgba[i + 2] = b;
      rgba[i + 3] = Math.round((hits / (S * S)) * 255);
    }
  }
  return encodePng(size, size, rgba);
}

const COLORS = {
  idle: [143, 150, 171], // grey  — no credentials
  ready: [91, 157, 255], // blue  — credentials loaded, no tunnels
  active: [63, 191, 127], // green — tunnels open
  warn: [232, 179, 57], // amber — starting, or expiring soon
  error: [240, 102, 95], // red   — a tunnel failed or credentials expired
};

const cache = new Map();

// Electron picks the right density from the representations we add, so the tray
// stays sharp on Retina and on Windows at 125%/150% scaling.
function icon(status) {
  if (cache.has(status)) return cache.get(status);
  const color = COLORS[status] || COLORS.idle;
  const image = nativeImage.createFromBuffer(ring(16, color), { scaleFactor: 1 });
  image.addRepresentation({ scaleFactor: 2, buffer: ring(32, color) });
  image.addRepresentation({ scaleFactor: 3, buffer: ring(48, color) });
  cache.set(status, image);
  return image;
}

module.exports = { icon, encodePng, ring, COLORS };
