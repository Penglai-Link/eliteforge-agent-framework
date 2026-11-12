import crypto from 'node:crypto';
import { deflateRawSync } from 'node:zlib';

const crcTable = Array.from({ length: 256 }, (_, value) => {
  for (let bit = 0; bit < 8; bit++) value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
  return value >>> 0;
});
function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = crcTable[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

// Deterministic ZIP32 with UTF-8 paths and SHA-256 checksums expected by Framework.
export function archive(files) {
  const entries = new Map([...files].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0));
  const checksums = Object.fromEntries([...entries].map(([name, data]) => [name, crypto.createHash('sha256').update(data).digest('hex')]));
  entries.set('checksums.json', Buffer.from(JSON.stringify(checksums)));
  const local = [], central = [];
  let offset = 0, total = 0;
  if (entries.size > 10000) throw new Error('包内文件超过 10000 个');
  for (const [name, content] of entries) {
    const data = Buffer.from(content), filename = Buffer.from(name);
    total += data.length;
    if (data.length > 128 * 1024 * 1024 || total > 256 * 1024 * 1024) throw new Error('包超过 Framework 大小限制');
    const compressed = deflateRawSync(data), crc = crc32(data);
    const header = Buffer.alloc(30);
    header.writeUInt32LE(0x04034b50, 0);
    header.writeUInt16LE(20, 4);
    header.writeUInt16LE(0x800, 6);
    header.writeUInt16LE(8, 8);
    header.writeUInt16LE(33, 12); // 1980-01-01, reproducible timestamps.
    header.writeUInt32LE(crc, 14);
    header.writeUInt32LE(compressed.length, 18);
    header.writeUInt32LE(data.length, 22);
    header.writeUInt16LE(filename.length, 26);
    const directory = Buffer.alloc(46);
    directory.writeUInt32LE(0x02014b50, 0);
    directory.writeUInt16LE(0x314, 4);
    header.copy(directory, 6, 4, 28);
    directory.writeUInt32LE(((name.startsWith('bin/') ? 0o100755 : 0o100644) << 16) >>> 0, 38);
    directory.writeUInt32LE(offset, 42);
    local.push(header, filename, compressed);
    central.push(directory, filename);
    offset += header.length + filename.length + compressed.length;
  }
  const directorySize = central.reduce((size, buffer) => size + buffer.length, 0);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.size, 8);
  end.writeUInt16LE(entries.size, 10);
  end.writeUInt32LE(directorySize, 12);
  end.writeUInt32LE(offset, 16);
  if (offset + directorySize + end.length > 128 * 1024 * 1024) throw new Error('ZIP 超过 128 MiB');
  return Buffer.concat([...local, ...central, end]);
}
