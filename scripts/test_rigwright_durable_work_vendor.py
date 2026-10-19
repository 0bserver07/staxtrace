"""Offline RP-139 byte identity, dependency boundary and SQLx patch checks."""
import hashlib
import json
from pathlib import Path
import re
import tarfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "vendor/rigwright-durable-work"
SQLX = ROOT / "vendor/sqlx-postgres-only"


def postgres_manifest(original):
    """Remove unused SQLite resolution edges, without changing any version/source.

    Both SQLx's facade and its macros-core declare an optional SQLite driver.
    Cargo resolves that driver's native `links` even in this Postgres-only graph.
    The upstream tests/dev-deps are not part of these dependency-only copies.
    """
    parsed = tomllib.loads(original)
    sections = re.split(r"(?m)^(\[.+\]\n)", original)
    result = sections[0]
    for header, body in zip(sections[1::2], sections[2::2]):
        if (header.startswith(("[[test]]", "[[bench]]", "[dev-dependencies.",
                               "[dependencies.sqlx-sqlite]", "[package.metadata.docs.rs]"))):
            continue
        if header == "[features]\n":
            body = "".join(
                f"{key} = {json.dumps([v for v in values if 'sqlite' not in v])}\n"
                for key, values in parsed["features"].items()
            )
        if header == "[lints.rust.unexpected_cfgs]\n":
            # Declare the upstream compiler cfg names for this path dependency.
            body += 'check-cfg = ["cfg(sqlx_macros_unstable)", "cfg(procmacro2_semver_exempt)"]\n'
        result += header + body
    return "# RP-139: PostgreSQL-only resolution patch; all upstream source bytes unchanged.\n" + result


class VendorContract(unittest.TestCase):
    def test_queue_archive_and_every_extracted_byte_match_the_candidate(self):
        pin = json.loads((VENDOR / "candidate.json").read_text())
        path = VENDOR / "deskroost-jobs-0.1.0.tar"
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        self.assertEqual(digest, pin["archive_sha256"])
        self.assertEqual((VENDOR / "SHA256SUMS").read_text(),
                         f"{digest}  {path.name}\n")
        with tarfile.open(path) as archive:
            names = set()
            for member in archive:
                self.assertTrue(member.isfile())
                data = archive.extractfile(member).read()
                name = member.name.split("/", 1)[1]
                names.add(name)
                self.assertEqual(data, (VENDOR / member.name).read_bytes())
                self.assertEqual(hashlib.sha256(data).hexdigest(), pin["files"][name])
            self.assertEqual(names, set(pin["files"]))
        extracted = VENDOR / "deskroost-jobs-0.1.0"
        self.assertEqual(
            {path.relative_to(extracted).as_posix()
             for path in extracted.rglob("*") if path.is_file()},
            names,
        )
        self.assertNotIn("LICENSE", names)
        self.assertNotIn("NOTICE", names)
        package = tomllib.loads((extracted / "Cargo.toml").read_text())["package"]
        self.assertNotIn("license", package)
        self.assertNotIn("license-file", package)

    def test_feature_is_optional_exact_and_has_no_other_rigwright_dependency(self):
        manifest = tomllib.loads((ROOT / "rust/crates/stax-server/Cargo.toml").read_text())
        deps = manifest["dependencies"]
        self.assertEqual(deps["deskroost-jobs"], {
            "version": "=0.1.0", "optional": True,
            "path": "../../../vendor/rigwright-durable-work/deskroost-jobs-0.1.0",
        })
        self.assertNotIn("default", manifest["features"])
        self.assertIn("dep:deskroost-jobs", manifest["features"]["rigwright-durable-work"])
        self.assertTrue(deps["sqlx"]["optional"])
        self.assertEqual([name for name in deps if name.startswith(("deskroost-", "rigwright-"))],
                         ["deskroost-jobs"])
        binary = next(b for b in manifest["bin"] if b["name"] == "stax-durable-backfill")
        self.assertEqual(binary["required-features"], ["rigwright-durable-work"])
        candidate = tomllib.loads((VENDOR / "deskroost-jobs-0.1.0/Cargo.toml").read_text())
        for name, value in candidate["dependencies"].items():
            self.assertFalse(name.startswith(("deskroost-", "rigwright-")))
            if isinstance(value, dict):
                self.assertFalse({"path", "git", "workspace"} & value.keys())

    def test_sqlx_patches_remove_sqlite_edges_and_declare_upstream_cfgs_without_source_changes(self):
        pins = json.loads((SQLX / "upstream.json").read_text())
        for package, pin in pins.items():
            original = (SQLX / package / "Cargo.toml.upstream").read_text()
            self.assertEqual(hashlib.sha256(original.encode()).hexdigest(),
                             pin["manifest_without_author_contacts_sha256"])
            self.assertEqual((SQLX / package / "Cargo.toml").read_text(),
                             postgres_manifest(original))
            patched = tomllib.loads(postgres_manifest(original))
            self.assertEqual(patched["package"]["version"], tomllib.loads(original)["package"]["version"])
            self.assertNotIn("sqlx-sqlite", patched["dependencies"])
            self.assertFalse(any("sqlite" in v for values in patched["features"].values() for v in values))
            for name, digest in pin["files"].items():
                self.assertEqual(hashlib.sha256((SQLX / package / name).read_bytes()).hexdigest(), digest)


if __name__ == "__main__":
    unittest.main()
