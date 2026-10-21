"""Read and bind an installed Cairo reference without evaluating its Ruby recipe."""
import hashlib
import json
from pathlib import Path
import re

LIBRARIES = ("libcairo.2.dylib", "libcairo-gobject.2.dylib", "libcairo-script-interpreter.2.dylib")
PINS = Path(__file__).with_name("cairo-source.json")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def stable_fields(text):
    """Accept literal stable header fields, not conditional Ruby source selection."""
    fields, header, skipped = {}, True, []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if skipped:
            indent = len(line) - len(line.lstrip(" "))
            if line.strip() == "end" and indent == skipped[-1]:
                skipped.pop()
            elif re.fullmatch(r" +on_[a-z0-9_]+ do", line) and indent > skipped[-1]:
                skipped.append(indent)
            elif (indent <= skipped[-1] or ";" in line
                    or re.match(r"\s*(?:end|if|unless|case|begin|def|class|module|while|until|for)\b", line)
                    or re.search(r"\bdo\b", line)):
                raise ValueError("unsupported metadata block boundary in Cairo recipe")
            continue
        # These DSL blocks describe non-stable sources/update checks or bottles.
        # Only standalone class-level blocks may be excluded from source scanning.
        if re.fullmatch(r"  (bottle|head|livecheck) do", line):
            skipped.append(2)
            continue
        if re.search(r"\b(url|sha256|version|revision)\b", line):
            match = re.fullmatch(r"  (url|sha256|version|revision)\b(.*)", line)
            if not header or not match:
                raise ValueError("conditional/dynamic source field in Cairo recipe")
            key, raw = match.groups()
            if key in fields:
                raise ValueError(f"ambiguous recipe field {key}")
            value = (re.fullmatch(r'\s+"([^"\n#{}]+)"\s*', raw) if key != "revision"
                     else re.fullmatch(r"\s+(\d+)\s*", raw))
            if not value:
                raise ValueError(f"nonliteral recipe field {key}")
            fields[key] = value[1]
        elif (line != "class Cairo < Formula"
              and not re.fullmatch(r'  (?:desc|homepage|head) "[^"#{}]+"(?:, branch: "[^"#{}]+")?', line)
              and not re.fullmatch(r'  license (?:"[^"#{}]+"|any_of: \["[^"#{}]+"(?:, "[^"#{}]+")*\])', line)):
            # Once executable/conditional DSL begins, later stable declarations
            # are not accepted, even if their indentation imitates the header.
            header = False
    if skipped:
        raise ValueError("unterminated metadata block in Cairo recipe")
    return fields


def read_reference(prefix):
    prefix = prefix.resolve()
    selected = prefix / "opt/cairo"
    actual = selected.resolve()
    context = f"Cairo reference {selected} -> {actual}"
    try:
        if not actual.is_dir() or actual.parent != prefix / "Cellar/cairo":
            raise ValueError("reference is not an installed Cairo keg")
        match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:_([1-9]\d*))?", actual.name)
        if not match:
            raise ValueError(f"unsupported keg version {actual.name}")
        version, revision = match[1], int(match[2] or 0)
        receipt_path, recipe = actual / "INSTALL_RECEIPT.json", actual / ".brew/cairo.rb"
        for path in (receipt_path, recipe, *(actual / "lib" / name for name in LIBRARIES)):
            if not path.is_file() or path.is_symlink() or not path.resolve().is_relative_to(actual):
                raise ValueError(f"missing/nonregular reference evidence: {path}")
        receipt = json.loads(receipt_path.read_text())
        stable = receipt.get("source", {}).get("versions", {}).get("stable")
        context += f"; keg={actual.name}; receipt stable={stable}"
        if (receipt.get("source", {}).get("spec") != "stable" or stable != version
                or receipt.get("arch") != "arm64"):
            raise ValueError("receipt stable version/spec/architecture does not match the keg")
        pins = {entry["version"]: entry for entry in json.loads(PINS.read_text())["sources"]}
        if version not in pins:
            raise ValueError(f"unsupported source version {version}; audited versions: {', '.join(sorted(pins))}")
        text = recipe.read_text()
        fields = stable_fields(text)
        if fields.get("version", version) != version or int(fields.get("revision", "0")) != revision:
            raise ValueError("recipe version/revision does not match the installed receipt/keg")
        if re.search(r"^\s*(?:patch|resource)\b|^__END__$", text, re.M):
            raise ValueError("Cairo recipe patches/resources require explicit build review")
        pin = pins[version]
        expected_url = f"https://cairographics.org/releases/cairo-{version}.tar.xz"
        if (pin["url"] != expected_url or fields.get("url") != expected_url
                or not re.fullmatch(r"[0-9a-f]{64}", fields.get("sha256", ""))
                or fields["sha256"] != pin["sha256"]):
            raise ValueError("installed recipe URL/SHA does not match the audited official source pin")
        source = {**pin, "version": actual.name, "source_version": version, "recipe_sha256": digest(recipe)}
        return {"version": version, "keg_version": actual.name, "revision": revision,
                "keg_path": str(actual), "recipe_sha256": digest(recipe),
                "receipt_sha256": digest(receipt_path), "source": source,
                "library_sha256": {name: digest(actual / "lib" / name) for name in LIBRARIES}}
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ValueError(f"{context}: {error}") from error


def require_reference(expected, prefix):
    actual = read_reference(prefix)
    if actual != expected:
        raise ValueError(f"Cairo reference changed: current keg={actual['keg_version']}, version={actual['version']}; recipe/receipt/library evidence must match")
    return actual


def source_lock(reference):
    return {"format": 1, "cairo_reference": reference, "sources": [reference["source"]]}


def source_filename(reference):
    entry = reference["source"]
    return entry["sha256"] + "-cairo-" + entry["version"] + ".source"


def verify_build_report(report, reference):
    if (report.get("reference") != reference or report.get("version") != reference["version"]
            or report.get("source_sha256") != reference["source"]["sha256"]
            or report.get("lzo") != "disabled" or not report.get("script_interpreter_preserved")
            or "-Dlzo=disabled" not in report.get("options", [])):
        raise ValueError("Cairo build report source/version/recipe/reference/configuration mismatch; rebuild using the current reference lock")
    for name in LIBRARIES:
        item = report.get("libraries", {}).get(name, {})
        if (item.get("reference_sha256") != reference["library_sha256"][name]
                or item.get("architecture") != "arm64" or item.get("missing_symbols") != [] or item.get("original_symbols", 0) <= 0
                or item.get("rebuilt_symbols", 0) < item.get("original_symbols", 0)
                or not re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", ""))):
            raise ValueError(f"Cairo build report has invalid ABI/hash evidence: {name}")
