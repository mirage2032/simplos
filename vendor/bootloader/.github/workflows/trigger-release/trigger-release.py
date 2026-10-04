import sys
import toml
import requests
import subprocess

cargo_toml = toml.load("Cargo.toml")
crate_version = cargo_toml["workspace"]["package"]["version"]
print("Detected crate version " + crate_version)

# crates.io enforces a data-access policy that requires a descriptive
# User-Agent with contact info; requests without one get rejected, which
# previously made this script misclassify already-published versions as
# unreleased. See https://crates.io/data-access.
headers = {
    "User-Agent": "bootloader-release-ci (https://github.com/rust-osdev/bootloader)"
}
api_url = "https://crates.io/api/v1/crates/bootloader/" + crate_version
response = requests.get(api_url, headers=headers)

if response.status_code == 200 and "version" in response.json():
    version = response.json()["version"]
    assert (version["crate"] == "bootloader")
    assert (version["num"] == crate_version)
    print("Version " + crate_version + " already exists on crates.io")

elif response.status_code == 404:
    print("Could not find version " + crate_version +
          " on crates.io; creating a new release")

    tag_name = "v" + crate_version
    sha = subprocess.run(["git", "rev-parse", "HEAD"], check=True,
                         stdout=subprocess.PIPE).stdout.decode("utf-8").strip()
    print(f"  Tagging commit {sha} as {tag_name}")

    command = [
        "gh", "api", "--method", "POST", "-H", "Accept: application/vnd.github+json",
        "/repos/rust-osdev/bootloader/releases",
        "-f", f"tag_name={tag_name}", "-f", f"target_commitish={sha}",
        "-f", f"name={tag_name}",
        "-f", "body=[Changelog](https://github.com/rust-osdev/bootloader/blob/main/Changelog.md)",
        "-F", "draft=false", "-F", "prerelease=false", "-F", "generate_release_notes=false",
    ]
    print("  Running `" + ' '.join(command) + '`')
    result = subprocess.run(command, capture_output=True, text=True)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode != 0:
        # A release for this tag may already exist (e.g. the version was
        # published on crates.io but the crates.io check briefly failed, or
        # this workflow is being re-run). Treat that as a no-op instead of a
        # hard failure; fail loudly on any other error.
        if "already_exists" in result.stdout or "already_exists" in result.stderr:
            print(f"  Release {tag_name} already exists; nothing to do")
        else:
            raise SystemExit(result.returncode)
    else:
        print("  Done")

else:
    # Any other response (rate limiting, data-access policy rejection, 5xx,
    # etc.) is ambiguous: we can't tell whether the version is published.
    # Fail loudly rather than guessing and attempting to re-create a release.
    raise SystemExit(
        f"Unexpected response from crates.io (HTTP {response.status_code}): "
        f"{response.text}"
    )
