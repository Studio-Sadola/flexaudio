// 依存ポリシー: 許可集合へ足せるのは、Windows 自身が提供する根拠がある DLL だけ。
// 根拠つきの一覧は tranext の build/check-win-pe-imports.mjs にある OS_RESOLVED_DLL_NAMES。
// 報告を黙らせるために許可集合を広げない。禁止の柄は許可より強く、誤って許可集合へ
// 足しても先に止める。

// 以下の英語の理由の文は tranext の build/check-win-pe-imports.mjs（2c6cf74）から逐語。
// DLLs Windows itself always resolves: importing one of these is never a missing dependency.
export const ALLOWED_DLL_NAMES = Object.freeze([
  // Win32 base + core COM/OLE: loaded into (or reachable from) every process.
  'kernel32.dll',
  // NT kernel services + the C runtime that ships with Windows (legacy msvcrt and the UCRT); ntdll/sechost/combase are forwarded-to core DLLs.
  'ntdll.dll',
  // Crypto and authentication: part of the OS, not a redistributable.
  'bcryptprimitives.dll',
  // Win32 base + core COM/OLE: loaded into (or reachable from) every process.
  'ole32.dll',
  // Win32 base + core COM/OLE: loaded into (or reachable from) every process.
  'oleaut32.dll',
  // mmdevapi.dll is the MMDevice API (Core Audio) and ships in System32 with Windows; it enumerates audio endpoints, which is why flexaudio's native module imports it (measured on the pinned win32-x64 .node, ab8df7f).
  'mmdevapi.dll',
  // Shell/UI and media helpers used by Electron and by native media stacks.
  'propsys.dll',
]);

// API sets. Windows 10 and later resolve every name in this namespace from the OS itself (the api-ms-win-* entries are virtual, not files on disk), so the prefix is matched instead of listing the hundreds of members.
export const API_SET_NAME_PREFIX = 'api-ms-win-';

/**
 * 禁止の柄。見つかったら非 0 で終了する。
 *
 *  - msvcp / vcruntime / vcomp: MSVC のランタイム。静的 CRT で組んである前提なので、
 *    これが import に現れる＝配布先に VC++ 再頒布可能パッケージが要る状態。
 *  - directml / onnxruntime: 推論ランタイム。実行時に同梱/配置が要る＝自己完結でない。
 *
 * 部分一致・大小文字無視で判定する（MSVCP140.dll / msvcp140.dll / MSVCP140_1.dll など）。
 */
export const FORBIDDEN_PATTERNS = ['msvcp', 'vcruntime', 'vcomp', 'directml', 'onnxruntime'];

const ALLOWED_DLL_NAME_SET = new Set(ALLOWED_DLL_NAMES);

/** 読み取った依存を禁止・未許可に分類する。禁止は未許可より優先する。 */
export function judgeDependencies(names) {
  const forbidden = [];
  const unexpected = [];

  for (const name of names) {
    const lower = name.toLowerCase();
    const pattern = FORBIDDEN_PATTERNS.find((candidate) => lower.includes(candidate));
    if (pattern !== undefined) {
      forbidden.push({ name, pattern });
    } else if (!ALLOWED_DLL_NAME_SET.has(lower) && !lower.startsWith(API_SET_NAME_PREFIX)) {
      unexpected.push(name);
    }
  }

  return { forbidden, unexpected };
}
