// Inspects the dependency DLLs loaded by a Windows PE (.node / .exe / .dll) by reading the PE
// headers directly, without relying on external tools. Used by the
// "Verify native addon dependencies (Windows)" step of release-npm.yml.
//
// Why not dumpbin: dumpbin ships with MSVC and is not on PATH unless the vcvars (Developer
// Command Prompt) environment is loaded. With the GitHub runner's default PATH only the check
// fails with "The term 'dumpbin' is not recognized ..." = **whether the check passes is
// decided by the tools on the runner, not by the artifact** (observed: run 35509560651; the
// build itself succeeded, but the failure of this check skipped the artifact upload).
// So this is done entirely with Node's standard features, independent of the environment.
//
// Usage:
//   node check-pe-dependencies.mjs <pe-file> [more-pe-files ...]
//
// Exit codes:
//   0 = every input could be read and every dependency is in the allowed set of
//       pe-dependency-policy.mjs
//   1 = a forbidden dependency or one not in the allowed set was found / an input could not be
//       read (not a PE, corrupt, missing file, zero arguments). **Never "pass because it could
//       not be read" (fail-closed)**. A check that cannot read silently misses the fact that a
//       dependency was added = worse than no check.
//
// What is read (both are always examined):
//   - the regular Import directory      (IMAGE_DIRECTORY_ENTRY_IMPORT = 1)
//   - the Delay-Import directory        (IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT = 13)
//   A delay import is merely "not loaded until that function is actually called"; it is still a
//   dependency that must ship with the distribution. A check that looks at only one is a hole.

import { readFileSync } from 'node:fs';
import { isReadableDllName } from './pe-dll-name.mjs';
import { judgeDependencies } from './pe-dependency-policy.mjs';

/** Machine values of the COFF header. */
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

// dlattrRva: bit indicating that addresses in the Delay-Import table are RVAs, not VAs.
const DLATTR_RVA = 0x1;

const IMPORT_DESCRIPTOR_SIZE = 20; // IMAGE_IMPORT_DESCRIPTOR
const DELAY_DESCRIPTOR_SIZE = 32; // IMAGE_DELAYLOAD_DESCRIPTOR
// Upper bound so we do not walk forever when the terminator (all fields 0) never comes.
const MAX_IMPORT_DESCRIPTORS = 4096;
const MAX_DELAY_DESCRIPTORS = 4096;
// DLL names are short. Anything with no NUL within this length is not a name.
const MAX_DLL_NAME_BYTES = 260;

/** Signals that the input could not be read as a PE. The caller turns it into a non-zero exit. */
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

/** Maps an RVA to a file offset. null if it cannot be mapped (no exception = leaves room for a VA retry). */
function rvaToOffset(image, rva) {
  if (!Number.isInteger(rva) || rva <= 0) return null;
  for (const section of image.sections) {
    const span = Math.max(section.virtualSize, section.sizeOfRawData);
    if (rva >= section.virtualAddress && rva < section.virtualAddress + span) {
      const delta = rva - section.virtualAddress;
      // Beyond SizeOfRawData there is nothing in the file (a region zero-filled at load time).
      if (delta >= section.sizeOfRawData) return null;
      const offset = section.pointerToRawData + delta;
      return offset < image.buf.length ? offset : null;
    }
  }
  // Before the first section = the header region, where the RVA equals the file position.
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

/** Reads a NUL-terminated DLL name at the given offset. null if it is not a valid name. */
function readDllNameAt(image, offset) {
  if (offset === null) return null;
  const end = Math.min(image.buf.length, offset + MAX_DLL_NAME_BYTES);
  let text = '';
  for (let i = offset; i < end; i += 1) {
    const byte = image.buf[i];
    if (byte === 0) return isReadableDllName(text) ? text : null;
    // Contains something other than printable ASCII = this is not a name.
    if (byte < 0x20 || byte > 0x7e) return null;
    text += String.fromCharCode(byte);
  }
  return null; // Not NUL-terminated.
}

/**
 * Resolves an address value to a DLL name.
 *
 * Old-format Delay-Import tables point to the DLL name by **VA** (an absolute address that
 * includes ImageBase) instead of an RVA. In principle the dlattrRva bit tells them apart, but
 * in real binaries the flag cannot always be trusted. So "try the intended interpretation first,
 * and if it does not resolve, retry with the other interpretation (for a VA, the value minus
 * ImageBase as an RVA)". Implementing only one silently returns zero entries and the check
 * passes through = the worst way to break.
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
    // A descriptor with all fields 0 is the terminator.
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

    // Without dlattrRva it is the old format = the fields are VAs. But the flag is not taken at
    // face value: if resolution fails, resolveDllName retries with the other interpretation.
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

/** Reads the headers of one image (DOS → PE → COFF → optional header → section table). */
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
    // Look only at the smaller of the declared count and the area actually present.
    numberOfRvaAndSizes: Math.min(declaredDirs, availableDirs),
    dataDirectoriesOffset: optional + dirsRelative,
  };
}

function describeMachine(machine) {
  const name = MACHINE_TYPES.get(machine);
  const hex = `0x${machine.toString(16).padStart(4, '0')}`;
  return name === undefined ? `UNKNOWN (${hex})` : `${name} (${hex})`;
}

/** Does not list the same DLL twice (case-insensitive; formatting for display only). */
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

/** Inspects one file. Failures are thrown; the caller turns them into a non-zero exit. */
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
    // Always print the dependency list, pass or fail (so the log shows what the verdict was based on).
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
        // Parsing succeeded but not a single entry could be read = the import table is likely
        // being misread. Passing here would make it "a broken check that always passes".
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
