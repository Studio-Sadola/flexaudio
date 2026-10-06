// Dependency policy: add a DLL to the allowlist only when there is evidence that Windows itself provides it.
// The evidence-backed list is OS_RESOLVED_DLL_NAMES in tranext's build/check-win-pe-imports.mjs.
// Do not expand the allowlist to silence reports. Forbidden patterns take precedence over allowed
// names, so they are caught even if mistakenly added to the allowlist.

// The English reason comments below are copied verbatim from tranext's build/check-win-pe-imports.mjs (2c6cf74).
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
 * Forbidden patterns. Exit with a nonzero status if any are found.
 *
 *  - msvcp / vcruntime / vcomp: MSVC runtimes. The build is expected to use the static CRT, so
 *    any such import means the recipient needs the VC++ Redistributable.
 *  - directml / onnxruntime: inference runtimes that must be bundled or installed at runtime,
 *    which means the package is not self-contained.
 *
 * Match case-insensitively by substring (for example, MSVCP140.dll, msvcp140.dll, or MSVCP140_1.dll).
 */
export const FORBIDDEN_PATTERNS = ['msvcp', 'vcruntime', 'vcomp', 'directml', 'onnxruntime'];

const ALLOWED_DLL_NAME_SET = new Set(ALLOWED_DLL_NAMES);

/** Classify read dependencies as forbidden or unexpected. Forbidden takes precedence. */
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
