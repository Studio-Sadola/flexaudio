const DOS_HEADER_SIZE = 0x40;
const SECTION_RVA = 0x1000;
const SECTION_RAW = 0x400;
const COFF_HEADER_SIZE = 20;
const DATA_DIRECTORY_COUNT = 16;
const IMPORT_DESCRIPTOR_SIZE = 20;
const DELAY_DESCRIPTOR_SIZE = 32;
const IMPORT_NAME_RVA_OFFSET = 12;
const DELAY_NAME_RVA_OFFSET = 4;
const IMPORT_DIR_INDEX = 1;
const DELAY_IMPORT_DIR_INDEX = 13;
const PE32_PLUS_MAGIC = 0x20b;
const PE32_PLUS_RVA_COUNT_OFFSET = 108;
const OPTIONAL_HEADER_SIZE = PE32_PLUS_RVA_COUNT_OFFSET + 4 + DATA_DIRECTORY_COUNT * 8;
const COFF_OFFSET = DOS_HEADER_SIZE + 4;
const OPTIONAL_OFFSET = COFF_OFFSET + COFF_HEADER_SIZE;
const SECTION_TABLE_OFFSET = OPTIONAL_OFFSET + OPTIONAL_HEADER_SIZE;
const DATA_DIRECTORY_OFFSET = OPTIONAL_OFFSET + PE32_PLUS_RVA_COUNT_OFFSET + 4;

function rvaOf(offset) {
  return SECTION_RVA + (offset - SECTION_RAW);
}

function placeNames(names, cursor) {
  const placed = [];
  let next = cursor;
  for (const name of names) {
    placed.push({ name, offset: next, rva: rvaOf(next) });
    next += Buffer.byteLength(name, 'ascii') + 1;
  }
  return { placed, end: next };
}

/** Build a minimal PE32+ image with normal and delay-import DLL descriptors. */
export function buildPe({ imports = [], delayImports = [] } = {}) {
  const importTableOffset = SECTION_RAW;
  const importTableSize = (imports.length + 1) * IMPORT_DESCRIPTOR_SIZE;
  const delayTableOffset = importTableOffset + importTableSize;
  const delayTableSize = delayImports.length === 0 ? 0 : (delayImports.length + 1) * DELAY_DESCRIPTOR_SIZE;
  // llvm-objdump walks the INT/IAT while printing imports. A shared, empty
  // zero-terminated thunk table makes the descriptors well-formed without
  // adding function imports that this fixture does not need to model.
  const thunkTableOffset = delayTableOffset + delayTableSize;
  const thunkTableSize = imports.length === 0 && delayImports.length === 0 ? 0 : 8;
  const thunkTableRva = thunkTableSize === 0 ? 0 : rvaOf(thunkTableOffset);
  const importNames = placeNames(imports, thunkTableOffset + thunkTableSize);
  const delayNames = placeNames(delayImports, importNames.end);
  const end = Math.max(delayNames.end, delayTableOffset + delayTableSize);
  const buf = Buffer.alloc(Math.max(SECTION_RAW + 16, Math.ceil(end / 16) * 16), 0);
  const rawSize = buf.length - SECTION_RAW;

  buf.write('MZ', 0, 'ascii');
  buf.writeUInt32LE(DOS_HEADER_SIZE, 0x3c);
  buf.writeUInt32LE(0x4550, DOS_HEADER_SIZE); // "PE\0\0"
  buf.writeUInt16LE(0x8664, COFF_OFFSET); // machine = AMD64
  buf.writeUInt16LE(1, COFF_OFFSET + 2); // one section
  buf.writeUInt16LE(OPTIONAL_HEADER_SIZE, COFF_OFFSET + 16);
  buf.writeUInt16LE(PE32_PLUS_MAGIC, OPTIONAL_OFFSET);
  buf.writeUInt32LE(DATA_DIRECTORY_COUNT, OPTIONAL_OFFSET + PE32_PLUS_RVA_COUNT_OFFSET);
  buf.write('.rdata', SECTION_TABLE_OFFSET, 'ascii');
  buf.writeUInt32LE(rawSize, SECTION_TABLE_OFFSET + 8);
  buf.writeUInt32LE(SECTION_RVA, SECTION_TABLE_OFFSET + 12);
  buf.writeUInt32LE(rawSize, SECTION_TABLE_OFFSET + 16);
  buf.writeUInt32LE(SECTION_RAW, SECTION_TABLE_OFFSET + 20);
  buf.writeUInt32LE(rvaOf(importTableOffset), DATA_DIRECTORY_OFFSET + IMPORT_DIR_INDEX * 8);
  buf.writeUInt32LE(importTableSize, DATA_DIRECTORY_OFFSET + IMPORT_DIR_INDEX * 8 + 4);

  if (delayTableSize !== 0) {
    buf.writeUInt32LE(rvaOf(delayTableOffset), DATA_DIRECTORY_OFFSET + DELAY_IMPORT_DIR_INDEX * 8);
    buf.writeUInt32LE(delayTableSize, DATA_DIRECTORY_OFFSET + DELAY_IMPORT_DIR_INDEX * 8 + 4);
  }

  for (const [index, entry] of importNames.placed.entries()) {
    const descriptor = importTableOffset + index * IMPORT_DESCRIPTOR_SIZE;
    buf.writeUInt32LE(thunkTableRva, descriptor);
    buf.writeUInt32LE(entry.rva, descriptor + IMPORT_NAME_RVA_OFFSET);
    buf.writeUInt32LE(thunkTableRva, descriptor + 16);
    buf.write(entry.name, entry.offset, 'ascii');
  }

  for (const [index, entry] of delayNames.placed.entries()) {
    const descriptor = delayTableOffset + index * DELAY_DESCRIPTOR_SIZE;
    buf.writeUInt32LE(1, descriptor); // dlattrRva: delay-import name is an RVA.
    buf.writeUInt32LE(entry.rva, descriptor + DELAY_NAME_RVA_OFFSET);
    buf.writeUInt32LE(thunkTableRva, descriptor + 12);
    buf.writeUInt32LE(thunkTableRva, descriptor + 16);
    buf.write(entry.name, entry.offset, 'ascii');
  }

  return buf;
}
