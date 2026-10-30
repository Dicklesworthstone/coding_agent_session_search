#!/usr/bin/env python3
"""Run and record supplemental GH538 component tests, never the full CASS gate."""

import argparse
import datetime
import json
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import time
import tomllib
from pathlib import Path

from extract import BASELINE, HERE, NEGATIVE_ASSERTION, NEGATIVE_TEST, REPO, extract, sha256


def run_process(command, cwd, env, log_prefix, timeout):
    stdout = log_prefix.with_suffix(".stdout.log")
    stderr = log_prefix.with_suffix(".stderr.log")
    started = time.monotonic()
    timed_out = False
    with stdout.open("wb") as out, stderr.open("wb") as err:
        process = subprocess.Popen(
            command, cwd=cwd, env=env, stdout=out, stderr=err, start_new_session=True
        )
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            # Only the process group created above belongs to this invocation.
            os.killpg(process.pid, signal.SIGKILL)
            code = process.wait()
    return {
        "command": [str(part) for part in command],
        "cwd": str(cwd),
        "returncode": code,
        "timed_out": timed_out,
        "wall_seconds": round(time.monotonic() - started, 3),
        "stdout": stdout.name,
        "stdout_sha256": sha256(stdout.read_bytes()),
        "stderr": stderr.name,
        "stderr_sha256": sha256(stderr.read_bytes()),
    }


def log_text(logs, result):
    return "\n".join((logs / result[key]).read_text(errors="replace")
                     for key in ("stdout", "stderr"))


def cargo_configuration(crate, env):
    directories = [crate, *crate.parents]
    candidates = [directory / ".cargo" / name
                  for directory in directories for name in ("config", "config.toml")]
    cargo_home = Path(env.get("CARGO_HOME", str(Path.home() / ".cargo")))
    if not cargo_home.is_absolute():
        cargo_home = crate / cargo_home
    cargo_home = cargo_home.resolve()
    candidates.extend(cargo_home / name for name in ("config", "config.toml"))
    return {str(path): sha256(path.read_bytes()) for path in candidates if path.is_file()}


def run_component(name, repo, output, provenance, env, cargo, target, offline, timeout):
    crate = output / name
    logs = output / "logs"
    result = {"component": name, "status": "failed", "source_verified_before": False,
              "source_verified_after": False}
    extract(repo, output, verify=True)
    result["source_verified_before"] = True
    result["sources_sha256"] = sha256((crate / "sources.json").read_bytes())
    build_env = dict(env, CARGO_TARGET_DIR=str(target))
    result["cargo_configuration_sha256"] = cargo_configuration(crate, build_env)
    command = [cargo, "test", "--locked", "--lib", "--no-run", "--message-format=json", "-j1"]
    if offline:
        command.append("--offline")
    print(f"Building {name} with the recorded source and dependency identities", flush=True)
    build = run_process(command, crate, build_env, logs / (name + "-build"), timeout)
    result["build"] = build
    if build["returncode"] != 0:
        result["failure"] = "component compilation failed; no tests claimed"
        return result
    artifacts = []
    for line in (logs / build["stdout"]).read_text().splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (message.get("reason") == "compiler-artifact"
                and message.get("profile", {}).get("test")
                and message.get("executable")):
            artifacts.append(message)
    if len(artifacts) != 1:
        result["failure"] = f"expected one Cargo test executable, found {len(artifacts)}"
        return result
    artifact = artifacts[0]
    binary = Path(artifact["executable"]).resolve()
    result["executable"] = str(binary)
    result["executable_sha256"] = sha256(binary.read_bytes())
    result["cargo_artifact_fresh"] = artifact.get("fresh")
    result["package_id"] = artifact.get("package_id")

    listing = run_process([str(binary), "--list"], crate, build_env,
                          logs / (name + "-list"), 30)
    result["test_inventory"] = listing
    listed = (logs / listing["stdout"]).read_text()
    names = [line.removesuffix(": test") for line in listed.splitlines() if line.endswith(": test")]
    expected = {name.rsplit("::", 1)[-1] for name in provenance["selected_tests"]}
    if (listing["returncode"] != 0 or len(names) != len(expected)
            or {name.rsplit("::", 1)[-1] for name in names} != expected):
        result["failure"] = "compiled test inventory differs from the extracted selection"
        return result
    result["compiled_tests"] = names
    test_command = [str(binary), "--nocapture", "--test-threads=1"]
    if name == "mirror-baseline":
        test_command += ["--exact", NEGATIVE_TEST]
        result["purpose"] = "pinned pre-fix negative control; one specific assertion must fail"
    print(f"Running {name}", flush=True)
    test = run_process(test_command, crate, build_env, logs / (name + "-tests"), 120)
    result["tests"] = test
    text = log_text(logs, test)
    summary = re.search(
        r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
        r"(\d+) measured; (\d+) filtered out", text
    )
    if not summary:
        result["failure"] = "missing complete libtest result"
    else:
        status, passed, failed, ignored, measured, filtered = summary.groups()
        counts = dict(zip(("passed", "failed", "ignored", "measured", "filtered_out"),
                          map(int, (passed, failed, ignored, measured, filtered))))
        result["counts"] = counts
        if name == "mirror-baseline":
            matched = (
                test["returncode"] == 101 and status == "FAILED"
                and counts == {"passed": 0, "failed": 1, "ignored": 0, "measured": 0,
                               "filtered_out": len(expected) - 1}
                and f"test {NEGATIVE_TEST} ... FAILED" in text
                and NEGATIVE_ASSERTION in text
            )
            result["status"] = "expected-regression-reproduced" if matched else "failed"
        else:
            matched = (
                test["returncode"] == 0 and status == "ok"
                and counts == {"passed": len(expected), "failed": 0, "ignored": 0,
                               "measured": 0, "filtered_out": 0}
            )
            result["status"] = "passed" if matched else "failed"
        if not matched:
            result["failure"] = "test result did not satisfy this component's explicit expectation"
    extract(repo, output, verify=True)
    result["source_verified_after"] = True
    if cargo_configuration(crate, build_env) != result["cargo_configuration_sha256"]:
        raise ValueError("Cargo configuration changed during the component run")
    if sha256(binary.read_bytes()) != result["executable_sha256"]:
        raise ValueError("test binary changed during run")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True,
                        help="explicit directory for generated crates, logs, and report.json")
    parser.add_argument("--repo", type=Path, default=REPO)
    parser.add_argument("--offline", action="store_true", help="require all locked dependencies cached")
    parser.add_argument("--toolchain-bin", type=Path,
                        help="optional directory containing the pinned cargo and rustc binaries")
    parser.add_argument("--mirror-target-dir", type=Path)
    parser.add_argument("--watchdog-target-dir", type=Path)
    parser.add_argument("--build-timeout", type=int, default=900)
    args = parser.parse_args()
    if os.name != "posix" or not shutil.which("mkfifo"):
        parser.error("the real FIFO and symlink probes require a POSIX host with mkfifo")
    repo, output_root = args.repo.resolve(), args.output_dir.resolve()
    started_utc = datetime.datetime.now(datetime.timezone.utc)
    run_id = started_utc.strftime("%Y%m%dT%H%M%S.%fZ") + f"-{os.getpid()}"
    output = output_root / "runs" / run_id
    output.mkdir(parents=True, exist_ok=False)
    (output / "logs").mkdir(exist_ok=True)
    env = os.environ.copy()
    if args.toolchain_bin:
        env["PATH"] = str(args.toolchain_bin.resolve()) + os.pathsep + env.get("PATH", "")
    pin = tomllib.loads((repo / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    env["RUSTUP_TOOLCHAIN"] = pin
    env["CARGO_BUILD_JOBS"] = "1"
    env.setdefault("RUST_MIN_STACK", "134217728")
    env.setdefault("RUSTFLAGS", "-Z threads=1")
    cargo, rustc = (shutil.which(name, path=env.get("PATH")) for name in ("cargo", "rustc"))
    if not cargo or not rustc:
        parser.error("cargo and rustc are required; install the repository pin or use --toolchain-bin")
    # Cargo must compile with the same rustc whose identity we report, even if
    # the caller has an inherited compiler override or build.rustc setting.
    env["RUSTC"] = rustc
    env["CARGO_BUILD_RUSTC"] = rustc
    report = {
        "format": 1,
        "qualification": "supplemental component evidence, not the full Rust/live-watcher/macOS gate",
        "started_utc": started_utc.isoformat(),
        "run_id": run_id,
        "evidence_directory": str(output),
        "baseline_revision": BASELINE,
        "candidate_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo).decode().strip(),
        "platform": platform.platform(),
        "toolchain_pin": pin,
        "cargo": subprocess.check_output([cargo, "--version", "--verbose"], env=env).decode().strip(),
        "rustc": subprocess.check_output([rustc, "--version", "--verbose"], env=env).decode().strip(),
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "environment": {key: env.get(key) for key in (
            "RUSTUP_TOOLCHAIN", "RUSTC", "CARGO_BUILD_RUSTC", "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS", "RUST_MIN_STACK",
            "CARGO_BUILD_JOBS", "CARGO_HOME", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER")},
        "components": [],
        "status": "failed",
    }
    try:
        provenance = extract(repo, output)
        report["source_verification"] = "generated and verified before and after each test run"
        for name in ("mirror-candidate", "mirror-baseline", "watchdog"):
            target = (args.watchdog_target_dir if name == "watchdog" else args.mirror_target_dir)
            target = (target or output_root / "target").resolve()
            result = run_component(name, repo, output, provenance[name], env, cargo, target,
                                   args.offline, args.build_timeout)
            result["scope"] = provenance[name]["scope"]
            result["exclusions"] = provenance[name]["exclusions"]
            report["components"].append(result)
            print(f"{name}: {result['status']}", flush=True)
            (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if all(component["status"] in ("passed", "expected-regression-reproduced")
               for component in report["components"]):
            report["status"] = "supplemental-passed"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        report["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        path = output / "report.json"
        path.write_text(json.dumps(report, indent=2) + "\n")
        (output_root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{report['status']}: {path}", flush=True)
    return 0 if report["status"] == "supplemental-passed" else 1


if __name__ == "__main__":
    sys.exit(main())
