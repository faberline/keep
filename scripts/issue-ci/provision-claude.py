"""Disabled review candidate: verify official fixed-version CLI, never run it.

No installer shell script, package manager, credential, arbitrary URL or version
argument. Current checked-in supply is disabled and contains no invented hashes.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile
import urllib.request

VERSION = "2.1.310"
ORIGIN = "https://downloads.claude.ai"
FINGERPRINT = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE"
ROOT = Path(__file__).resolve().parents[2]


class Refused(RuntimeError):
    pass


def need(ok, code):
    if not ok:
        raise Refused(code)


def unique(pairs):
    value = {}
    for key, item in pairs:
        need(key not in value, "duplicate_json_key")
        value[key] = item
    return value


def decode(raw):
    return json.loads(raw, object_pairs_hook=unique)


def validate(supply):
    need(type(supply.get("version")) is int and supply["version"] == 1, "supply_version")
    need(supply.get("enabled") is True and supply.get("status") == "reviewed_pinned", "supply_not_enabled")
    expected = dict(cli_version=VERSION, platform="linux-x64", origin=ORIGIN,
        manifest_path=f"/claude-code-releases/{VERSION}/manifest.json",
        signature_path=f"/claude-code-releases/{VERSION}/manifest.json.sig",
        binary_path=f"/claude-code-releases/{VERSION}/linux-x64/claude",
        signing_key_path="/keys/claude-code.asc", signing_fingerprint=FINGERPRINT)
    need(all(supply.get(key) == value for key, value in expected.items()), "fixed_official_source_required")
    for key in ("manifest_sha256", "binary_sha256"):
        need(isinstance(supply.get(key), str) and re.fullmatch(r"[0-9a-f]{64}", supply[key]), "reviewed_supply_hash_missing")
    return supply


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise Refused("supply_redirect_refused")


def fetch(path, limit):
    need(path in (f"/claude-code-releases/{VERSION}/manifest.json",
                  f"/claude-code-releases/{VERSION}/manifest.json.sig",
                  f"/claude-code-releases/{VERSION}/linux-x64/claude", "/keys/claude-code.asc"), "source_path_refused")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(ORIGIN + path, timeout=30) as response:
        need(response.status == 200 and response.geturl() == ORIGIN + path, "supply_response_refused")
        raw = response.read(limit + 1)
    need(len(raw) <= limit, "supply_size_refused")
    return raw


def signature_valid(directory, manifest, signature, key, execute=subprocess.run):
    home = Path(directory)
    for name, raw in (("manifest.json", manifest), ("manifest.json.sig", signature), ("release.asc", key)):
        (home / name).write_bytes(raw)
    common = ["/usr/bin/gpg", "--batch", "--no-options", "--homedir", str(home), "--no-auto-key-retrieve"]
    env = dict(HOME=str(home), PATH="/usr/bin:/bin")
    imported = execute(common + ["--import", str(home / "release.asc")], stdout=subprocess.PIPE,
                       stderr=subprocess.DEVNULL, env=env, timeout=30, check=False)
    need(imported.returncode == 0, "signing_key_import_failed")
    verified = execute(common + ["--status-fd", "1", "--verify", str(home / "manifest.json.sig"), str(home / "manifest.json")],
                       stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, timeout=30, check=False)
    statuses = [line.split() for line in verified.stdout.decode("ascii", errors="strict").splitlines()
                if line.startswith("[GNUPG:] VALIDSIG ")]
    need(verified.returncode == 0 and len(statuses) == 1 and len(statuses[0]) in (11, 12)
         and (statuses[0][2] == FINGERPRINT or statuses[0][-1] == FINGERPRINT), "manifest_signature_refused")


def provision(supply, destination, reader=fetch, verify=signature_valid):
    validate(supply)  # No network/write occurs for disabled or incomplete supply.
    manifest = reader(supply["manifest_path"], 1048576)
    need(hashlib.sha256(manifest).hexdigest() == supply["manifest_sha256"], "manifest_hash_mismatch")
    signature = reader(supply["signature_path"], 1048576)
    key = reader(supply["signing_key_path"], 1048576)
    with tempfile.TemporaryDirectory(prefix="claude-manifest-verify-") as directory:
        verify(directory, manifest, signature, key)
    value = decode(manifest)
    need(value.get("version") == VERSION and value.get("platforms", {}).get("linux-x64", {}).get("checksum") == supply["binary_sha256"],
         "signed_platform_hash_mismatch")
    binary = reader(supply["binary_path"], 512 * 1024 * 1024)
    need(hashlib.sha256(binary).hexdigest() == supply["binary_sha256"], "binary_hash_mismatch")
    need(not Path(destination).exists(), "existing_destination_refused")
    with open(destination, "xb") as stream:
        stream.write(binary)
    Path(destination).chmod(0o700)
    return str(destination)


def main():
    need(sys.argv[1:] == [], "arguments_refused")
    need(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_OS") == "Linux"
         and platform.machine() == "x86_64", "ephemeral_linux_x64_runner_required")
    need(not any(os.environ.get(key) for key in ("GH_TOKEN", "GITHUB_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY")),
         "credentialed_provisioning_refused")
    supply = validate(decode((ROOT / ".github/claude-review-cli-supply.json").read_bytes()))
    with tempfile.TemporaryDirectory(prefix="claude-supply-stage-", dir=os.environ["RUNNER_TEMP"]) as temporary:
        staged = provision(supply, Path(temporary) / "claude")
        # Persist only the verified binary for the next fixed install step.
        destination = Path(os.environ["RUNNER_TEMP"]) / "claude-reviewed-2.1.310"
        need(not destination.exists(), "existing_destination_refused")
        os.rename(staged, destination)
    need("\n" not in str(destination) and "\r" not in str(destination), "output_path_refused")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        stream.write("binary=" + str(destination) + "\n")
    print("verified_official_cli_staged_not_executed")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Refused as error:
        print("refused: " + str(error), file=sys.stderr)
        sys.exit(2)
    except Exception:
        print("refused: cli_supply_unknown_no_execution", file=sys.stderr)
        sys.exit(2)
