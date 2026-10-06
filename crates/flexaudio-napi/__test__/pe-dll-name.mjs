// Validate that the resolved value looks like a dependency DLL name. Treating arbitrary bytes
// as a name would prevent detection of an RVA/VA mix-up.
// llvm-mingw dynamically references the C++ standard library as libc++.dll.
const DLL_NAME_RE = /^[A-Za-z0-9_.+-]{1,255}\.dll$/i;

/** Check whether a DLL name has a safe format for reading as a PE import descriptor name. */
export function isReadableDllName(name) {
  return typeof name === 'string' && DLL_NAME_RE.test(name);
}
