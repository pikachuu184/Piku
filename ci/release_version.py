"""Calculate the next release version from commits since the latest semver tag."""

import re
import subprocess


def run(*args: str) -> str:
    return subprocess.check_output(args, text=True).strip()


tags = run("git", "tag", "--list", "v[0-9]*", "--sort=-version:refname").splitlines()
latest = tags[0] if tags else "v0.1.0"
base = latest[1:]
major, minor, patch = (int(part) for part in base.split("."))
commits = run("git", "log", f"{latest}..HEAD", "--format=%s%n%b") if tags else run("git", "log", "--format=%s%n%b")

if re.search(r"BREAKING CHANGE|!:", commits):
    major += 1
    minor = patch = 0
elif re.search(r"(^|\n)feat(\([^)]*\))?:", commits):
    minor += 1
    patch = 0
else:
    patch += 1

print(f"{major}.{minor}.{patch}")
