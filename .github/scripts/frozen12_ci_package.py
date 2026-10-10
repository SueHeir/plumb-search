#!/usr/bin/env python3
"""CI-only: focused synthetic checks and two ordinary binaries; no corpus run."""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import struct
import subprocess
import sys
import time

BASELINE = "63c508d83d437308a6c0171f4ee60d1da7889b9a"
REPOSITORY = "SueHeir/plumb-search"
EXECUTABLE_CAP = 2 * 1024**3
METADATA_CAP = 512 * 1024
PACKAGE_CAP = 2 * EXECUTABLE_CAP + 1024**2
ALLOWLIST = ("baseline-frozen12", "candidate-frozen12", "attestation.json",
             "SHA256SUMS", "allowlist-audit.json")
ADAPTER_TESTS = {
    "frozen12_binding_mismatch_and_incomplete_baseline_are_rejected",
    "frozen12_failure_clears_all_observations_and_retains_unknown_status",
    "frozen12_negative_empty_retrieval_and_external_queries",
    "frozen12_raw_stronger_page_guard_does_not_replace_homepage",
    "frozen12_request_options_and_cohort_are_fixed",
    "frozen12_router_secondary_place_lookup_preserves_bound_primary_raw",
    "frozen12_router_swallowed_secondary_failure_marks_report_incomplete",
    "frozen12_router_uses_real_retrieval_and_keeps_unknown_observations",
}
NAVIGATION_TESTS = {
    "task_navigation_selects_dining_shopping_and_combined_sections",
    "task_navigation_keeps_homepage_navigation_and_weak_or_unrelated_requests",
    "task_navigation_rejects_unrelated_labels_and_untrusted_addresses",
    "task_navigation_rejects_competing_sections_but_deduplicates_the_same_url",
    "task_navigation_yields_to_strong_retrieved_pages_and_preserves_row_budget",
    "task_navigation_frozen_pg10_regression_preserves_provenance_and_site_metadata",
    "task_navigation_mcp_exposes_selected_url_and_navigation_provenance",
    "task_navigation_api_html_and_click_redirect_choose_the_same_destination",
    "task_navigation_searxng_keeps_site_metadata_and_link_provenance",
}
RETAINED_TESTS = {
    "retained_denies_every_mutation_writer_lock_watch_and_path_escape",
    "retained_rejects_missing_changed_symlink_and_false_metadata_bindings",
    "retained_native_and_pages_search_without_directory_changes",
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def capture(args, cwd, cap=1024**2):
    result = subprocess.run(args, cwd=cwd, check=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=60)
    require(len(result.stdout) <= cap and len(result.stderr) <= cap,
            "metadata command output cap")
    return result.stdout


def text(args, cwd, cap=1024**2):
    return capture(args, cwd, cap).decode("utf-8").strip()


def file_sha(path, cap):
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_size <= cap,
            "not an allowed bounded regular file")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024**2):
            digest.update(chunk)
    after = path.lstat()
    require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns,
             before.st_ctime_ns) ==
            (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns,
             after.st_ctime_ns), "file changed during hashing")
    return digest.hexdigest()


def source_snapshot(root):
    require(not text(["git", "status", "--porcelain", "--untracked-files=all"], root),
            "source checkout must be clean, with no nonignored local files")
    revision = text(["git", "rev-parse", "HEAD"], root)
    tree = text(["git", "rev-parse", "HEAD^{tree}"], root)
    names = capture(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], root)
    paths = sorted(p for p in names.decode("utf-8").split("\0") if p)
    digest = hashlib.sha256()
    total = 0
    for name in paths:
        path = root / name
        require(stat.S_ISREG(path.lstat().st_mode), "tracked symlink/special file denied")
        data = path.read_bytes()
        total += len(data)
        require(total <= 64 * 1024**2, "public source size cap")
        digest.update(name.encode("utf-8") + b"\0")
        digest.update(struct.pack("<Q", len(data)))
        digest.update(data)
    return {"revision": revision, "tree": tree, "dirty": False,
            "source_sha256": digest.hexdigest(), "source_bytes": total,
            "tracked_file_count": len(paths),
            "digest_algorithm": "build.rs: sorted UTF-8 path,NUL,u64le byte length,file bytes",
            "Cargo_lock_sha256": file_sha(root / "Cargo.lock", 1024**2)}


def controlled_command(args, root, logfile, seconds, cap):
    started = time.monotonic()
    with logfile.open("xb") as stream:
        process = subprocess.Popen(args, cwd=root, stdout=stream,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while process.poll() is None:
                require(time.monotonic() - started <= seconds, "CI command wall cap")
                require(logfile.stat().st_size <= cap, "CI command log cap")
                time.sleep(0.25)
            require(process.returncode == 0, "CI command failed: " + args[0])
            require(logfile.stat().st_size <= cap, "CI command log cap")
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
    return logfile.read_text(encoding="utf-8")


def fixtures(root, work):
    suites = [
        ("adapter", ["cargo", "test", "--locked", "-p", "plumb-node", "--example", "frozen12",
                     "--", "--test-threads=1"], ADAPTER_TESTS),
        ("navigation", ["cargo", "test", "--locked", "-p", "plumb-node", "--lib", "task_navigation",
                        "--", "--test-threads=1"], NAVIGATION_TESTS),
        ("retained", ["cargo", "test", "--locked", "-p", "plumb-index", "--lib", "retained::tests",
                      "--", "--test-threads=1"], RETAINED_TESTS),
    ]
    receipt = {}
    for name, command, expected in suites:
        output = controlled_command(command, root, work / (name + ".log"), 600, 2 * 1024**2)
        passed = re.findall(r"^test ([\w:]+) \.\.\. ok$", output, re.MULTILINE)
        names = [name.rsplit("::", 1)[-1] for name in passed]
        require(len(names) == len(expected) and set(names) == expected,
                "focused fixture names/count differ: " + name)
        require(re.search(r"test result: ok\. " + str(len(expected)) +
                          r" passed; 0 failed; 0 ignored; 0 measured;", output),
                "focused fixture summary differs: " + name)
        receipt[name] = {"command": command, "passed": len(names), "failed": 0,
                         "names": sorted(names)}
    return receipt


def toolchain(root):
    rustc = text(["rustc", "-vV"], root)
    cargo = text(["cargo", "-Vv"], root)
    require("release: 1.96.1\n" in rustc + "\n" and cargo.startswith("cargo 1.96.1 "),
            "both builds require pinned Rust/Cargo1.96.1")
    require("host: x86_64-unknown-linux-gnu" in rustc, "unexpected build target")
    sysroot = Path(text(["rustc", "--print", "sysroot"], root))
    binaries = {"rustc": sysroot / "bin/rustc", "cargo": sysroot / "bin/cargo",
                "rust_lld": sysroot / "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld"}
    return {"rustc_verbose": rustc, "cargo_verbose": cargo,
            "linker_version": text([str(binaries["rust_lld"]), "-flavor", "gnu", "--version"], root, 65536),
            "tool_binary_sha256": {name: file_sha(path.resolve(), 512 * 1024**2)
                                   for name, path in binaries.items()},
            "target": "x86_64-unknown-linux-gnu", "profile": "default dev/debug/incremental",
            "jobs": 1, "RUSTFLAGS": "unset", "PLUMB_PRIVATE_DIR": "unset",
            "private_wasm_inputs": "absent; empty generated payloads verified",
            "bundled_linker_identity_not_full_invocation_trace": True,
            "cache_action_incremental_override": "known0 cleared; default dev incremental applies",
            "runtime_compatibility": "ELF requirements attested; target-host compatibility must be verified before any run"}


def elf_metadata(path, root):
    header = text(["readelf", "-hW", str(path)], root, 65536)
    require("ELF64" in header and "Advanced Micro Devices X86-64" in header,
            "ordinary executable must be Linux ELF64 x86-64")
    program = text(["readelf", "-lW", str(path)], root, 65536)
    dynamic = text(["readelf", "-dW", str(path)], root, 65536)
    versions = text(["readelf", "--version-info", "-W", str(path)], root)
    notes = text(["readelf", "-nW", str(path)], root, 65536)
    return {"class": "ELF64", "machine": "x86-64",
            "interpreter": re.findall(r"Requesting program interpreter: ([^\]]+)", program),
            "needed_shared_libraries": re.findall(r"\(NEEDED\).*\[([^\]]+)\]", dynamic),
            "required_version_names": sorted(set(re.findall(r"Name: ((?:GLIBC|GLIBCXX|CXXABI|GCC)_[\w.]+)", versions))),
            "gnu_build_id": re.findall(r"Build ID: ([a-f0-9]+)", notes)}


def ordinary_build(root, work, stage, role, expected_toolchain):
    before = source_snapshot(root)
    require(toolchain(root) == expected_toolchain, "toolchain drift")
    for name in ("plumb_private.js", "plumb_private_bg.wasm"):
        require(not os.path.lexists(root / "target/private" / name), "foreign private-Wasm input denied")
    output = controlled_command(["cargo", "build", "--locked", "-p", "plumb-node", "--example",
                                 "frozen12", "--message-format=json"], root,
                                work / (role + "-build.jsonl"), 900, 16 * 1024**2)
    messages = []
    for line in output.splitlines():
        if line.startswith("{"):
            messages.append(json.loads(line))
    artifacts = [m for m in messages if m.get("reason") == "compiler-artifact" and
                 m.get("target", {}).get("name") == "frozen12" and
                 "example" in m.get("target", {}).get("kind", []) and
                 not m.get("profile", {}).get("test") and m.get("executable")]
    require(len(artifacts) == 1, "one ordinary frozen12 compiler artifact required")
    artifact = artifacts[0]
    profile = {key: artifact["profile"][key] for key in
               ("opt_level", "debuginfo", "debug_assertions", "overflow_checks", "test")}
    require(profile == {"opt_level": "0", "debuginfo": 2, "debug_assertions": True,
                        "overflow_checks": True, "test": False}, "default dev profile changed")
    executable = Path(artifact["executable"])
    require(executable == root / "target/debug/examples/frozen12", "unexpected executable path")
    scripts = [m for m in messages if m.get("reason") == "build-script-executed" and
               m.get("package_id") == artifact["package_id"]]
    require(len(scripts) == 1, "node build-script identity required")
    embedded = dict(scripts[0]["env"])
    for name, value in {"PLUMB_BUILD_REVISION": before["revision"], "PLUMB_BUILD_DIRTY": "false",
                        "PLUMB_BUILD_SOURCE": "git", "PLUMB_BUILD_SOURCE_SHA256": before["source_sha256"]}.items():
        require(embedded.get(name) == value, "compile-time source identity mismatch")
    out = Path(scripts[0]["out_dir"])
    require(out.resolve().is_relative_to((root / "target").resolve()), "foreign build output")
    for name in ("plumb_private.js", "plumb_private_bg.wasm"):
        require(stat.S_ISREG((out / name).lstat().st_mode) and (out / name).stat().st_size == 0,
                "nonempty private-Wasm payload denied")
    require(source_snapshot(root) == before, "source drift during build")
    original_sha = file_sha(executable, EXECUTABLE_CAP)
    destination = stage / (role + "-frozen12")
    with executable.open("rb") as source, destination.open("xb") as target:
        while chunk := source.read(1024**2):
            target.write(chunk)
    destination.chmod(0o755)
    require(file_sha(destination, EXECUTABLE_CAP) == original_sha == file_sha(executable, EXECUTABLE_CAP),
            "executable changed while staging")
    return {"source": before, "build_command": ["cargo", "build", "--locked", "-p", "plumb-node",
                                                "--example", "frozen12", "--message-format=json"],
            "compiler_artifact_profile": profile,
            "embedded_identity_build_script_env": {key: embedded[key] for key in
                ("PLUMB_BUILD_REVISION", "PLUMB_BUILD_DIRTY", "PLUMB_BUILD_SOURCE", "PLUMB_BUILD_SOURCE_SHA256")},
            "file": destination.name, "bytes": destination.stat().st_size,
            "sha256": original_sha, "mode": "0755", "elf": elf_metadata(destination, root),
            "ordinary_runner_executed": False}


def write_json(path, value):
    data = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    require(len(data) <= METADATA_CAP, "attestation metadata cap")
    with path.open("xb") as stream:
        stream.write(data)


def audit(stage):
    require(set(p.name for p in stage.iterdir()) == set(ALLOWLIST), "artifact inventory not exact allowlist")
    total = 0
    for name in ALLOWLIST:
        path = stage / name
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1,
                "artifact must be independent regular file, no symlink/hardlink")
        cap = EXECUTABLE_CAP if name.endswith("-frozen12") else METADATA_CAP
        file_sha(path, cap)
        total += info.st_size
    require(total <= PACKAGE_CAP, "aggregate artifact cap")
    return total


def main():
    root = Path.cwd().resolve()
    require(sys.platform == "linux", "existing Linux Actions runner only")
    require(os.environ.get("GITHUB_REPOSITORY") == REPOSITORY, "only approved public repository")
    expected = os.environ.get("FROZEN12_EXPECTED_HEAD", "")
    require(re.fullmatch(r"[a-f0-9]{40}", expected), "exact candidate head required")
    require(text(["git", "rev-parse", "HEAD"], root) == expected, "must build actual reviewed PR head, not merge ref")
    require(os.environ.get("CARGO_INCREMENTAL") in (None, "0"),
            "unexpected incremental override")
    # The existing pinned rust-cache action exports0; restore default dev settings.
    os.environ.pop("CARGO_INCREMENTAL", None)
    for name in os.environ:
        require(not name.startswith("PLUMB_") and not name.startswith("CARGO_PROFILE_") and
                name not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_INCREMENTAL"),
                "build override denied")
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    require(not any(os.path.lexists(cargo_home / name) for name in ("config", "config.toml")),
            "unreviewed global Cargo configuration denied")
    os.environ.update(CARGO_BUILD_JOBS="1", CARGO_TARGET_DIR=str(root / "target"), CARGO_TERM_COLOR="never")
    driver_bytes = Path(__file__).read_bytes()
    driver_sha256 = hashlib.sha256(driver_bytes).hexdigest()
    candidate = source_snapshot(root)
    require(candidate["revision"] != BASELINE, "candidate composition required")
    require("crates/plumb-node/src/assembly/navigation.rs" in
            text(["git", "ls-files"], root), "candidate navigation contribution absent")
    selected_toolchain = toolchain(root)
    work = root / "target/frozen12-ci-work"
    stage = root / "target/frozen12-ci-artifact"
    work.mkdir(parents=True, exist_ok=False)
    stage.mkdir(parents=True, exist_ok=False)
    with (work / "driver.py").open("xb") as snapshot:
        snapshot.write(driver_bytes)
    (work / "driver.py").chmod(0o400)
    focused = fixtures(root, work)
    require(source_snapshot(root) == candidate, "source drift after synthetic fixtures")
    subprocess.run(["git", "fetch", "--no-tags", "--depth=1",
                    "https://github.com/SueHeir/plumb-search.git", BASELINE], cwd=root, check=True)
    try:
        subprocess.run(["git", "checkout", "--detach", BASELINE], cwd=root, check=True)
        baseline = ordinary_build(root, work, stage, "baseline", selected_toolchain)
    finally:
        subprocess.run(["git", "checkout", "--detach", expected], cwd=root, check=True)
    require(source_snapshot(root) == candidate, "candidate identity not restored exactly")
    require(file_sha(root / ".github/scripts/frozen12_ci_package.py", 65536) == driver_sha256,
            "candidate driver bytes not restored")
    contribution = ordinary_build(root, work, stage, "candidate", selected_toolchain)
    require(source_snapshot(root) == candidate, "candidate source changed")
    write_json(stage / "attestation.json", {
        "schema": "frozen12-public-CI-package/1", "repository": REPOSITORY,
        "github_run_id": os.environ.get("GITHUB_RUN_ID"),
        "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "toolchain": selected_toolchain, "driver_sha256": driver_sha256,
        "focused_candidate_fixtures": focused,
        "baseline": baseline, "candidate": contribution,
        "allowed_scope": "public code, synthetic tests and two ordinary executable builds only",
        "corpus_or_real_reports_read": False, "ordinary_runner_executed": False,
        "source_archive_or_Git_history_uploaded": False})
    sums = "".join(file_sha(stage / name, EXECUTABLE_CAP if name.endswith("-frozen12") else METADATA_CAP) +
                   "  " + name + "\n" for name in ALLOWLIST[:3])
    with (stage / "SHA256SUMS").open("x") as stream:
        stream.write(sums)
    write_json(stage / "allowlist-audit.json", {"schema": "frozen12-artifact-allowlist/1",
        "exact_names": list(ALLOWLIST), "source_archive": False, "Git_history": False,
        "corpus": False, "real_reports": False, "settings_or_keys": False,
        "untracked_source_files": False, "metadata_cap_bytes_each": METADATA_CAP,
        "executable_cap_bytes_each": EXECUTABLE_CAP, "package_cap_bytes": PACKAGE_CAP,
        "sha256_of_other_files": {name: file_sha(stage / name, EXECUTABLE_CAP if name.endswith("-frozen12") else METADATA_CAP)
                                  for name in ALLOWLIST[:-1]}})
    total = audit(stage)
    require(source_snapshot(root) == candidate, "final clean-source audit failed")
    print("Audited exact five-file public CI package:", total, "bytes; no real runner execution")


if __name__ == "__main__":
    main()
