// Check dependent DLLs loaded by Windows PE files (.node / .exe / .dll) without external tools,
// by reading PE headers directly. Used by the "Verify native addon dependencies (Windows)" step
// in release-npm.yml.
//
// Why not use dumpbin: it ships with MSVC and is not on PATH unless the vcvars (Developer Command
// Prompt) environment is loaded. On the GitHub runner's default PATH, the check fails with
// "The term 'dumpbin' is not recognized ...". **That makes the check depend on runner tooling instead
// of the artifact** (observed in run 35509560651: the build succeeded, but this check failure skipped
// artifact upload). Use only Node built-ins here to make the check environment-independent.
//
// Usage:
//   node check-pe-dependencies.mjs <pe-file> [more-pe-files ...]
//
// Exit codes:
//   0 = All inputs were read, and every dependency is in the allowlist in pe-dependency-policy.mjs.
//   1 = A dependency is forbidden or not allowlisted, or an input could not be read (not a PE,
//       corrupt, missing, or no arguments). **Never pass because a file could not be read (fail-closed).**
//       An unreadable check silently misses new dependencies, which is worse than having no check.
//
// Read both of these:
//   - Normal Import directory        (IMAGE_DIRECTORY_ENTRY_IMPORT = 1)
//   - Delay-Import directory         (IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT = 13)
// Delay-imports are loaded only when a function is called, but they are still dependencies that
// must ship with the artifact. Checking only one directory leaves a gap.

import { readFileSync } from 'node:fs';
import { isReadableDllName } from './pe-dll-name.mjs';
import { judgeDependencies } from './pe-dependency-policy.mjs';

/** Machine value from the COFF header. */
const MACHINE_TYPES = new Map([
  [0x014c, 'I386'],
  [0x8664, 'AMD64'],
  [0x01c0, 'ARM'],
  [0x01c4, 'ARMNT'],
  [0xaa64, 'ARM64'],
  [0xa641, 'ARM64EC'],
  [0xa64e, 'ARM64X'],
]);

// Data directory indices (IMAGE_DIRECTORY_ENTRY_*).
const DIR_IMPORT = 1;
const DIR_DELAY_IMPORT = 13;

const PE32_MAGIC = 0x10b;
const PE32_PLUS_MAGIC = 0x20b;

// dlattrRva: bit indicating addresses in the Delay-Import table are RVAs rather than VAs.
const DLATTR_RVA = 0x1;

const IMPORT_DESCRIPTOR_SIZE = 20; // IMAGE_IMPORT_DESCRIPTOR
const DELAY_DESCRIPTOR_SIZE = 32; // IMAGE_DELAYLOAD_DESCRIPTOR
// Upper bound to avoid walking forever if no terminator (all fields zero) appears.
const MAX_IMPORT_DESCRIPTORS = 4096;
const MAX_DELAY_DESCRIPTORS = 4096;
// DLL names are short. If no NUL appears within this limit, the value is not a name.
const MAX_DLL_NAME_BYTES = 260;

/** Indicates that a file could not be read as PE. The caller converts this to a nonzero exit code. */
class PeError extends Error {}

function need(buf, offset, length, what) {
  if (!Number.isInteger(offset) || offset < 0 || offset + length > buf.length) {
    throw new PeError(
      `${what} at file offset 0x${Number(offset).toString(16)} (+${length} bytes) is outside the file (${buf.length} bytes)`,
    );
  }
}

function u16(buf, offset) {
  need(buf, offset, 2, 'uint16 field');
  return buf.readUInt16LE(offset);
}

function u32(buf, offset) {
  need(buf, offset, 4, 'uint32 field');
  return buf.readUInt32LE(offset);
}

function isZeroRange(buf, offset, length) {
  for (let i = offset; i < offset + length; i += 1) {
    if (buf[i] !== 0) return false;
  }
  return true;
}

/** Map an RVA to a file offset. Return null if it cannot be mapped (rather than throwing, allowing a VA retry). */
function rvaToOffset(image, rva) {
  if (!Number.isInteger(rva) || rva <= 0) return null;
  for (const section of image.sections) {
    const span = Math.max(section.virtualSize, section.sizeOfRawData);
    if (rva >= section.virtualAddress && rva < section.virtualAddress + span) {
      const delta = rva - section.virtualAddress;
      // Data beyond SizeOfRawData does not exist in the file (it is zero-filled when loaded).
      if (delta >= section.sizeOfRawData) return null;
      const offset = section.pointerToRawData + delta;
      return offset < image.buf.length ? offset : null;
    }
  }
  // Before the first section (the header area), the RVA equals the file offset.
  if (image.sections.length > 0 && rva < image.sections[0].virtualAddress && rva < image.buf.length) {
    return rva;
  }
  return null;
}

function dataDirectory(image, index) {
  if (index >= image.numberOfRvaAndSizes) return null;
  const offset = image.dataDirectoriesOffset + index * 8;
  if (offset + 8 > image.buf.length) return null;
  return { rva: u32(image.buf, offset), size: u32(image.buf, offset + 4) };
}

/** Read a NUL-terminated DLL name at the given offset. Return null if it is not a valid name. */
function readDllNameAt(image, offset) {
  if (offset === null) return null;
  const end = Math.min(image.buf.length, offset + MAX_DLL_NAME_BYTES);
  let text = '';
  for (let i = offset; i < end; i += 1) {
    const byte = image.buf[i];
    if (byte === 0) return isReadableDllName(text) ? text : null;
    // Non-printable ASCII means this is not a name.
    if (byte < 0x20 || byte > 0x7e) return null;
    text += String.fromCharCode(byte);
  }
  return null; // Not NUL-terminated.
}

/**
 * Resolve an address value to a DLL name.
 *
 * Older Delay-Import tables point to the DLL name with a **VA** (an absolute address including
 * ImageBase), not an RVA. The dlattrRva bit is supposed to distinguish the formats, but some real
 * files have unreliable flags. Try the indicated interpretation first, then retry with the other
 * interpretation (for a VA, subtract ImageBase and treat the result as an RVA). Implementing only
 * one interpretation could silently return zero dependencies and let the check pass incorrectly.
 */
function resolveDllName(image, value, preferVa) {
  const candidates = preferVa ? [value - image.imageBase, value] : [value, value - image.imageBase];
  for (const rva of candidates) {
    const name = readDllNameAt(image, rvaToOffset(image, rva));
    if (name !== null) return name;
  }
  return null;
}

function readImportNames(image) {
  const dir = dataDirectory(image, DIR_IMPORT);
  if (dir === null || dir.rva === 0) return [];

  const base = rvaToOffset(image, dir.rva);
  if (base === null) {
    throw new PeError(`import directory RVA 0x${dir.rva.toString(16)} is outside any section`);
  }

  const names = [];
  for (let i = 0; i < MAX_IMPORT_DESCRIPTORS; i += 1) {
    const offset = base + i * IMPORT_DESCRIPTOR_SIZE;
    need(image.buf, offset, IMPORT_DESCRIPTOR_SIZE, 'import descriptor');
    // A descriptor with all fields zero is the terminator.
    if (isZeroRange(image.buf, offset, IMPORT_DESCRIPTOR_SIZE)) return names;

    const nameRva = u32(image.buf, offset + 12);
    const name = readDllNameAt(image, rvaToOffset(image, nameRva));
    if (name === null) {
      throw new PeError(
        `import descriptor #${i} points at RVA 0x${nameRva.toString(16)}, which is not a readable DLL name`,
      );
    }
    names.push(name);
  }
  throw new PeError(`import descriptor table is not terminated within ${MAX_IMPORT_DESCRIPTORS} entries`);
}

function readDelayImportNames(image) {
  const dir = dataDirectory(image, DIR_DELAY_IMPORT);
  if (dir === null || dir.rva === 0) return [];

  const base = rvaToOffset(image, dir.rva);
  if (base === null) {
    throw new PeError(`delay-import directory RVA 0x${dir.rva.toString(16)} is outside any section`);
  }

  const names = [];
  for (let i = 0; i < MAX_DELAY_DESCRIPTORS; i += 1) {
    const offset = base + i * DELAY_DESCRIPTOR_SIZE;
    need(image.buf, offset, DELAY_DESCRIPTOR_SIZE, 'delay-import descriptor');
    if (isZeroRange(image.buf, offset, DELAY_DESCRIPTOR_SIZE)) return names;

    const attributes = u32(image.buf, offset);
    const nameField = u32(image.buf, offset + 4);
    if (nameField === 0) {
      throw new PeError(`delay-import descriptor #${i} has a null DLL name address`);
    }

    // If dlattrRva is not set, the old format uses VAs. Do not trust the flag blindly:
    // if resolution fails, resolveDllName retries with the other interpretation.
    const preferVa = (attributes & DLATTR_RVA) === 0;
    const name = resolveDllName(image, nameField, preferVa);
    if (name === null) {
      throw new PeError(
        `delay-import descriptor #${i} points at 0x${nameField.toString(16)}, which resolves to a DLL name ` +
          `neither as an RVA nor as a VA (ImageBase 0x${image.imageBase.toString(16)})`,
      );
    }
    names.push(name);
  }
  throw new PeError(`delay-import table is not terminated within ${MAX_DELAY_DESCRIPTORS} entries`);
}

/** Read the headers for one image (DOS → PE → COFF → optional header → section table). */
function parseImage(buf) {
  need(buf, 0, 0x40, 'DOS header');
  if (u16(buf, 0) !== 0x5a4d) throw new PeError('missing "MZ" signature (not a PE image)');

  const peOffset = u32(buf, 0x3c);
  need(buf, peOffset, 24, 'PE signature and COFF header');
  if (u32(buf, peOffset) !== 0x00004550) throw new PeError('missing "PE\\0\\0" signature (not a PE image)');

  const coff = peOffset + 4;
  const machine = u16(buf, coff);
  const numberOfSections = u16(buf, coff + 2);
  const sizeOfOptionalHeader = u16(buf, coff + 16);

  const optional = coff + 20;
  need(buf, optional, sizeOfOptionalHeader, 'optional header');
  const magic = u16(buf, optional);
  if (magic !== PE32_MAGIC && magic !== PE32_PLUS_MAGIC) {
    throw new PeError(`unknown optional header magic 0x${magic.toString(16)} (not PE32/PE32+)`);
  }
  const is64 = magic === PE32_PLUS_MAGIC;
  const imageBase = is64 ? Number(buf.readBigUInt64LE(optional + 24)) : u32(buf, optional + 28);

  const dirsRelative = is64 ? 112 : 96;
  const declaredDirs = u32(buf, optional + dirsRelative - 4); // NumberOfRvaAndSizes
  const availableDirs = sizeOfOptionalHeader > dirsRelative ? Math.floor((sizeOfOptionalHeader - dirsRelative) / 8) : 0;

  if (numberOfSections === 0) throw new PeError('the image declares no section, so no RVA can be resolved');

  const sectionTable = optional + sizeOfOptionalHeader;
  need(buf, sectionTable, numberOfSections * 40, 'section table');
  const sections = [];
  for (let i = 0; i < numberOfSections; i += 1) {
    const offset = sectionTable + i * 40;
    sections.push({
      name: buf.toString('latin1', offset, offset + 8).replace(/\0.*$/s, ''),
      virtualSize: u32(buf, offset + 8),
      virtualAddress: u32(buf, offset + 12),
      sizeOfRawData: u32(buf, offset + 16),
      pointerToRawData: u32(buf, offset + 20),
    });
  }

  return {
    buf,
    machine,
    imageBase,
    sections,
    // Read only the smaller of the declared count and the available data.
    numberOfRvaAndSizes: Math.min(declaredDirs, availableDirs),
    dataDirectoriesOffset: optional + dirsRelative,
  };
}

function describeMachine(machine) {
  const name = MACHINE_TYPES.get(machine);
  const hex = `0x${machine.toString(16).padStart(4, '0')}`;
  return name === undefined ? `UNKNOWN (${hex})` : `${name} (${hex})`;
}

/** Do not list the same DLL twice (case-insensitive; display-only normalization). */
function dedupe(names) {
  const seen = new Set();
  const unique = [];
  for (const name of names) {
    const key = name.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    unique.push(name);
  }
  return unique;
}

function printList(label, names) {
  if (names.length === 0) {
    console.log(`   ${label}: (none)`);
    return;
  }
  console.log(`   ${label} (${names.length}):`);
  for (const name of names) console.log(`     - ${name}`);
}

function printReport(image, imported, delayed) {
  console.log(`   machine: ${describeMachine(image.machine)}`);
  printList('imported DLLs (import directory)', imported);
  printList('delayed DLLs (delay-import directory)', delayed);
}

/** Check one file. Return failures as exceptions; the caller converts them to a nonzero exit code. */
function inspect(file) {
  const buf = readFileSync(file);
  const image = parseImage(buf);
  const imported = dedupe(readImportNames(image));
  const delayed = dedupe(readDelayImportNames(image));
  return { image, imported, delayed };
}

function main(argv) {
  if (argv.length === 0) {
    console.error('usage: node check-pe-dependencies.mjs <pe-file> [more-pe-files ...]');
    console.error('FAIL: no input file given; refusing to pass a check that inspected nothing');
    return 1;
  }

  let failures = 0;
  const fail = (message) => {
    failures += 1;
    console.log(`   FAIL: ${message}`);
    console.error(`FAIL: ${message}`);
  };

  for (const file of argv) {
    // Always print the dependency list, whether the check passes or fails, so the log shows what was checked.
    console.log(`== ${file}`);
    try {
      const { image, imported, delayed } = inspect(file);
      printReport(image, imported, delayed);

      const all = [...imported, ...delayed];
      const { forbidden, unexpected } = judgeDependencies(all);
      if (forbidden.length > 0) {
        fail(
          `${file}: forbidden native dependency (${forbidden.map((hit) => `${hit.name} ~ /${hit.pattern}/`).join(', ')})`,
        );
      }
      if (unexpected.length > 0) {
        fail(
          `${file}: unexpected native dependency (${unexpected.join(', ')}) - not in ALLOWED_DLL_NAMES; add it there only with evidence that Windows itself provides it`,
        );
      }
      if (all.length === 0) {
        // Parsing succeeded but no imports were read, suggesting the import table was misinterpreted.
        // Passing here would make this a broken check that always succeeds.
        fail(`${file}: parsed as PE but no DLL dependency could be read (refusing to pass)`);
      } else if (forbidden.length === 0 && unexpected.length === 0) {
        console.log('   OK: all dependencies are allowed');
      }
    } catch (err) {
      fail(`${file}: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  if (failures > 0) {
    console.error(`FAILED: ${failures} of ${argv.length} file(s) did not pass the dependency check`);
    return 1;
  }
  console.log(`OK: ${argv.length} file(s) inspected, all dependencies allowed`);
  return 0;
}

process.exitCode = main(process.argv.slice(2));
