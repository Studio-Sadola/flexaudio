// A missing build capability is reported as a skip on every OS.
export function capabilitySkip(missing, reason) {
  if (!missing) return false;
  return reason;
}
