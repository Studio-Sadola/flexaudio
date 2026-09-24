// Dependency policy: only DLLs with evidence that Windows itself provides them may be added to
// the allowed set. The list with evidence is OS_RESOLVED_DLL_NAMES in tranext's
// build/check-win-pe-imports.mjs.
// Do not widen the allowed set to silence a report. Forbidden patterns are stronger than the
// allow list and stop a DLL first, even if it is mistakenly added to the allowed set.

// The English rationale sentences below are verbatim from tranext's
// build/check-win-pe-imports.mjs (2c6cf74).
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
 * Forbidden patterns. Exits non-zero if one is found.
 *
 *  - msvcp / vcruntime / vcomp: the MSVC runtime. The build assumes a static CRT, so this
 *    appearing in the imports = the target machine needs the VC++ Redistributable.
 *  - directml / onnxruntime: inference runtimes. They must be bundled/deployed at runtime =
 *    not self-contained.
 *
 * Matched by substring, case-insensitively (MSVCP140.dll / msvcp140.dll / MSVCP140_1.dll, etc.).
 */
export const FORBIDDEN_PATTERNS = ['msvcp', 'vcruntime', 'vcomp', 'directml', 'onnxruntime'];

const ALLOWED_DLL_NAME_SET = new Set(ALLOWED_DLL_NAMES);

/** Classifies the read dependencies as forbidden / not allowed. Forbidden takes precedence. */
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
