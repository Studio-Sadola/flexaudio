// Windows の PE（.node / .exe / .dll）が読み込む依存 DLL を、外部ツールに頼らず
// PE ヘッダを直接読んで検査する。release-npm.yml の
// 「Verify native addon dependencies (Windows)」ステップが使う。
//
// なぜ dumpbin を使わないか: dumpbin は MSVC 同梱で、vcvars（開発者コマンドプロンプト）
// の環境を読まないと PATH に載らない。GitHub ランナーの既定 PATH では
// 「The term 'dumpbin' is not recognized ...」で検査だけが落ちる＝**検査の成否が
// 成果物ではなくランナーの道具の有無で決まってしまう**（実測: run 35509560651。
// ビルド自体は成功していたのに、この検査の失敗で成果物のアップロードがスキップされた）。
// ここは Node の標準機能だけで完結させ、環境非依存にする。
//
// 使い方:
//   node check-pe-dependencies.mjs <pe-file> [more-pe-files ...]
//
// 終了コード:
//   0 = 全ての入力を読めて、依存が全て pe-dependency-policy.mjs の許可集合に入っている
//   1 = 禁止の依存・許可集合に無い依存があった / 読めなかった（PE でない・壊れている・
//       ファイルが無い・引数が 0 個）。**「読めないから合格」は絶対にしない（fail-closed）**。
//       読めない検査は、依存が増えた事実を静かに見逃す＝無いより悪い。
//
// 読み取り対象（両方を必ず見る）:
//   - 通常の Import ディレクトリ        (IMAGE_DIRECTORY_ENTRY_IMPORT = 1)
//   - Delay-Import ディレクトリ         (IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT = 13)
//   遅延インポートは「実際にその関数を呼ぶまでロードされない」だけで、配布物に
//   同梱が要る依存であることに変わりはない。片方だけ見る検査は穴になる。

import { readFileSync } from 'node:fs';
import { isReadableDllName } from './pe-dll-name.mjs';
import { judgeDependencies } from './pe-dependency-policy.mjs';

/** COFF ヘッダの Machine 値。 */
const MACHINE_TYPES = new Map([
  [0x014c, 'I386'],
  [0x8664, 'AMD64'],
  [0x01c0, 'ARM'],
  [0x01c4, 'ARMNT'],
  [0xaa64, 'ARM64'],
  [0xa641, 'ARM64EC'],
  [0xa64e, 'ARM64X'],
]);

// データディレクトリの添字（IMAGE_DIRECTORY_ENTRY_*）。
const DIR_IMPORT = 1;
const DIR_DELAY_IMPORT = 13;

const PE32_MAGIC = 0x10b;
const PE32_PLUS_MAGIC = 0x20b;

// dlattrRva: Delay-Import テーブル内のアドレスが VA ではなく RVA であることを示すビット。
const DLATTR_RVA = 0x1;

const IMPORT_DESCRIPTOR_SIZE = 20; // IMAGE_IMPORT_DESCRIPTOR
const DELAY_DESCRIPTOR_SIZE = 32; // IMAGE_DELAYLOAD_DESCRIPTOR
// 終端（全フィールド 0）が来ないまま延々と歩かないための上限。
const MAX_IMPORT_DESCRIPTORS = 4096;
const MAX_DELAY_DESCRIPTORS = 4096;
// DLL 名は短い。これを超えて NUL が来ない物は名前ではない。
const MAX_DLL_NAME_BYTES = 260;

/** PE として読めなかったことを表す。呼び出し側が非 0 終了に変換する。 */
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

/** RVA をファイル内オフセットへ写す。写せなければ null（例外にしない＝VA 再試行の余地を残す）。 */
function rvaToOffset(image, rva) {
  if (!Number.isInteger(rva) || rva <= 0) return null;
  for (const section of image.sections) {
    const span = Math.max(section.virtualSize, section.sizeOfRawData);
    if (rva >= section.virtualAddress && rva < section.virtualAddress + span) {
      const delta = rva - section.virtualAddress;
      // SizeOfRawData を超えた先はファイルに実体が無い（ロード時に 0 埋めされる領域）。
      if (delta >= section.sizeOfRawData) return null;
      const offset = section.pointerToRawData + delta;
      return offset < image.buf.length ? offset : null;
    }
  }
  // 最初のセクションより前＝ヘッダ領域は、RVA とファイル位置が一致する。
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

/** 与えられたオフセットから NUL 終端の DLL 名を読む。名前として妥当でなければ null。 */
function readDllNameAt(image, offset) {
  if (offset === null) return null;
  const end = Math.min(image.buf.length, offset + MAX_DLL_NAME_BYTES);
  let text = '';
  for (let i = offset; i < end; i += 1) {
    const byte = image.buf[i];
    if (byte === 0) return isReadableDllName(text) ? text : null;
    // 印字可能 ASCII 以外が混ざる＝そこは名前ではない。
    if (byte < 0x20 || byte > 0x7e) return null;
    text += String.fromCharCode(byte);
  }
  return null; // NUL で終わらない。
}

/**
 * アドレス値を DLL 名へ解決する。
 *
 * 古い形式の Delay-Import テーブルは、DLL 名を RVA ではなく **VA**（ImageBase 込みの
 * 絶対アドレス）で指す。本来は dlattrRva ビットで判別できる建前だが、実物には
 * フラグが当てにならない物がある。そこで「意図した解釈を先に試し、解決できなければ
 * もう一方の解釈（VA なら ImageBase を引いた値を RVA として）で再試行する」。
 * 片方しか実装しないと、静かに 0 件を返して検査が素通りする＝最悪の壊れ方になる。
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
    // 全フィールド 0 の記述子が終端。
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

    // dlattrRva が立っていなければ旧形式＝フィールドは VA。ただしフラグを鵜呑みにせず、
    // 解決に失敗したら resolveDllName がもう一方の解釈で再試行する。
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

/** 画像 1 つ分のヘッダを読む（DOS → PE → COFF → optional header → セクション表）。 */
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
    // 宣言数と実際に置かれている領域の小さい方だけを見る。
    numberOfRvaAndSizes: Math.min(declaredDirs, availableDirs),
    dataDirectoriesOffset: optional + dirsRelative,
  };
}

function describeMachine(machine) {
  const name = MACHINE_TYPES.get(machine);
  const hex = `0x${machine.toString(16).padStart(4, '0')}`;
  return name === undefined ? `UNKNOWN (${hex})` : `${name} (${hex})`;
}

/** 同じ DLL を 2 回並べない（大小文字は無視。表示のためだけの整形）。 */
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

/** 1 ファイルを検査する。失敗は例外で返し、呼び出し側が非 0 終了に変換する。 */
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
    // 依存の一覧は、通っても落ちても必ず出す（何を見て判定したかがログに残るように）。
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
        // パースは通ったのに 1 件も読めなかった＝インポート表を読み違えている疑い。
        // ここで通すと「常に合格する壊れた検査」になる。
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
