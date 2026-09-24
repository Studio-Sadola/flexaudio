// Checks whether a resolved result looks like a "dependency DLL name". Accepting a byte
// sequence that does not look like a name would make it impossible to detect an RVA/VA mix-up.
// llvm-mingw references the C++ standard library dynamically as libc++.dll.
const DLL_NAME_RE = /^[A-Za-z0-9_.+-]{1,255}\.dll$/i;

/** Determines whether a DLL name is in a form that can be safely read as a PE import descriptor name. */
export function isReadableDllName(name) {
  return typeof name === 'string' && DLL_NAME_RE.test(name);
}
