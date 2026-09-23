// 解決結果が「依存 DLL 名」に見えるかの検証。名前らしくないバイト列を
// 名前として採用してしまうと、RVA/VA の取り違えを検出できなくなる。
// llvm-mingw は C++ 標準ライブラリを libc++.dll として動的に参照する。
const DLL_NAME_RE = /^[A-Za-z0-9_.+-]{1,255}\.dll$/i;

/** DLL 名が PE import descriptor の名前として安全に読める形式かを判定する。 */
export function isReadableDllName(name) {
  return typeof name === 'string' && DLL_NAME_RE.test(name);
}
