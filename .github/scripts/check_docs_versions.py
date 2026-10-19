"""Check tracked installation examples and registry links using the standard library.

Historical version references are allowed only in CHANGELOG.md, under an ATX
heading named exactly History or Migration (including its subsections), or on a
line containing the whole word history, since, previously, or the phrase
migration from (case insensitive). Fenced examples do not create headings.
Historical exemptions never excuse broken relative registry links.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import posixpath
import re
import subprocess
from urllib.parse import unquote, urlsplit


class DocumentationCheckError(Exception):
    """Documentation could not be inspected or contains release mismatches."""


@dataclass(frozen=True)
class Finding:
    path: str
    line: int
    reason: str
    text: str

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.reason}: {self.text.strip()}"


PACKAGE = r"flexaudio(?:-[a-z0-9-]+)?"
PACKAGE_START = r"(?<![\w-])"
VERSION = r"[0-9]+\.[0-9]+(?:\.[0-9]+)?"
HISTORY = re.compile(r"\b(?:history|since|previously|migration\s+from)\b", re.I)
HEADING = re.compile(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$")
VERSION_REFERENCES = (
    re.compile(rf"{PACKAGE_START}[\"']?{PACKAGE}[\"']?\s*=\s*[\"']([^\"']+)[\"']"),
    re.compile(rf"{PACKAGE_START}[\"']?{PACKAGE}[\"']?\s*=\s*\{{[^}}]*?\bversion\s*=\s*[\"']([^\"']+)[\"']", re.S),
    re.compile(r"\bPin\s+to\s+`([^`]+)`", re.I),
    re.compile(rf"^[ \t]*(?:[>*/#]+\s*)?(?:flexaudio\s+)?Version\s+v?({VERSION})\b", re.I | re.M),
)
PIP_COMMAND = re.compile(r"\bpip(?:3)?\s+install\b[^\n]*")
PIP_PIN = re.compile(rf"{PACKAGE_START}flexaudio(?:\[[^\]]+\])?\s*((?:(?:===|==|~=|>=|<=|!=|>|<)\s*[^\s`\"',;]+\s*,?\s*)+)")
INSTALL_COMMANDS = (
    (re.compile(r"\bcargo\s+add\b[^\n]*"), re.compile(rf"{PACKAGE_START}{PACKAGE}@([^\s`\"']+)")),
    (re.compile(r"\bnpm\s+(?:install|i)\b[^\n]*"),
     re.compile(rf"@studio-sadola/{PACKAGE}@([^\s`\"']+)")),
    (PIP_COMMAND, PIP_PIN),
)
GITHUB_REF = re.compile(
    r"https?://github\.com/Studio-Sadola/flexaudio/(?:blob|tree)/([^/\s)#?]+)", re.I
)
# Inline/image links, reference definitions, autolinks, and HTML links/images.
LINK = re.compile(
    r"\]\(\s*(?:<([^>]+)>|([^\s)]+))"
    r"|^\s{0,3}\[[^\]]+\]:\s*(?:<([^>]+)>|(\S+))"
    r"|\b(?:href|src)\s*=\s*[\"']([^\"']+)[\"']"
    r"|<((?:https?://)[^>]+)>", re.I | re.M
)
DECLARATIONS = {
    "crates/flexaudio-napi/index.d.ts",
    "bindings/flexaudio-py/flexaudio.pyi",
    "crates/flexaudio-ffi/include/flexaudio.h",
}


def documentation_path(path: str) -> bool:
    candidate = Path(path)
    return (candidate.name in {"README.md", "README.ja.md", "CHANGELOG.md"}
            or (path.startswith("docs/") and candidate.suffix == ".md")
            or path in DECLARATIONS)


def tracked_documentation(repo_root: Path) -> list[str]:
    result = subprocess.run(
        ["git", "-C", str(repo_root), "ls-files", "--cached", "-z"],
        capture_output=True, check=False,
    )
    if result.returncode != 0:
        raise DocumentationCheckError("Cannot enumerate tracked documentation with git ls-files")
    return sorted({path for path in result.stdout.decode("utf-8").split("\0")
                   if path and documentation_path(path)})


def historical_lines(lines: list[str]) -> set[int]:
    result: set[int] = set()
    sections: list[tuple[int, bool]] = []
    fence: str | None = None
    for number, line in enumerate(lines, 1):
        marker = re.match(r"^\s*(?:\*\s*)?(`{3,}|~{3,})", line)
        if marker:
            token = marker.group(1)
            if fence is None:
                fence = token
            elif token[0] == fence[0] and len(token) >= len(fence):
                fence = None
        heading = HEADING.match(line) if fence is None else None
        if heading:
            level = len(heading.group(1))
            while sections and sections[-1][0] >= level:
                sections.pop()
            sections.append((level, heading.group(2).casefold() in {"history", "migration"}))
        if any(historical for _, historical in sections) or HISTORY.search(line):
            result.add(number)
    return result


def current_requirement(requirement: str, version: str, *, pip: bool = False) -> bool:
    minor = version.rsplit(".", 1)[0]
    major_number, minor_number = (int(part) for part in minor.split("."))
    upper = f"{major_number}.{minor_number + 1}"
    parts = requirement.strip().split(",")
    has_current = False
    for part in parts:
        match = re.fullmatch(rf"\s*([~^=<>!]*)(v?{VERSION})\s*", part)
        if match is None:
            return False
        operator, value = match.groups()
        if pip and operator == "<" and value in {upper, upper + ".0"}:
            continue
        if operator not in {"", "=", "==", "===", "^", "~", "~=", ">="}:
            return False
        if value not in {minor, version}:
            return False
        has_current = True
    return has_current


def check_document(path: str, content: str, version: str) -> list[Finding]:
    lines = content.splitlines()
    historical = historical_lines(lines)
    findings: list[Finding] = []
    seen: set[tuple[int, str]] = set()
    changelog = Path(path).name == "CHANGELOG.md"

    def report(number: int, reason: str) -> None:
        if (number, reason) not in seen:
            seen.add((number, reason))
            findings.append(Finding(path, number, reason, lines[number - 1]))

    def check_match(offset: int, requirement: str, *, pip: bool = False) -> None:
        number = content.count("\n", 0, offset) + 1
        if not changelog and number not in historical and not current_requirement(requirement, version, pip=pip):
            report(number, f"version reference must match {version} or {version.rsplit('.', 1)[0]}")

    for pattern in VERSION_REFERENCES:
        for match in pattern.finditer(content):
            check_match(match.start(1), match.group(1))
    for command_pattern, pin_pattern in INSTALL_COMMANDS:
        for command in command_pattern.finditer(content):
            for pin in pin_pattern.finditer(command.group()):
                check_match(command.start() + pin.start(1), pin.group(1), pip=pin_pattern is PIP_PIN)

    parts = Path(path).parts
    registry = len(parts) == 3 and parts[0] in {"crates", "bindings"} and parts[2] in {"README.md", "README.ja.md"}
    if registry:
        for match in GITHUB_REF.finditer(content):
            number = content.count("\n", 0, match.start()) + 1
            if number not in historical and match.group(1) != f"v{version}":
                report(number, f"repository link must use v{version}")
        package = "/".join(parts[:2])
        for match in LINK.finditer(content):
            target = next(group for group in match.groups() if group is not None)
            url = urlsplit(target)
            if url.scheme or url.netloc or not url.path:
                continue
            resolved = posixpath.normpath(posixpath.join(package, unquote(url.path)))
            if resolved != package and not resolved.startswith(package + "/"):
                report(content.count("\n", 0, match.start()) + 1,
                       "relative registry link leaves its package directory")
    return sorted(findings, key=lambda finding: (finding.line, finding.reason))


def check_documentation(repo_root: Path, version: str) -> None:
    findings: list[Finding] = []
    root = repo_root.resolve()
    for path in tracked_documentation(root):
        source = root / path
        if not source.resolve().is_relative_to(root):
            raise DocumentationCheckError(f"{path}: documentation resolves outside the repository")
        try:
            content = source.read_text(encoding="utf-8")
            findings.extend(check_document(path, content, version))
        except (OSError, UnicodeError, ValueError) as error:
            raise DocumentationCheckError(f"{path}: cannot inspect documentation ({type(error).__name__})") from error
    if findings:
        raise DocumentationCheckError("Documentation check failed:\n" + "\n".join(str(finding) for finding in findings))
