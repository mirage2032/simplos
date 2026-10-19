# What the branch rules do and don't enforce

Both branches have a ruleset that refuses **force-pushes and deletion** — the
two things that lose work irrecoverably. Repository admins can bypass them, so
you can't lock yourself out.

Neither enforces "must go through a pull request with a passing check", and that
is a deliberate trade, not an omission. Promote pushes the release commit
*straight* to both branches, and a `pull_request` or `required_status_checks`
rule rejects a direct push — including a clean fast-forward, and including one
from Actions itself:

    remote: - Changes must be made through a pull request.
    remote: - Required status check "check" is in progress.

The usual answer is to give the workflow's token a bypass, but a ruleset bypass
actor can only be an app on a repository owned by an *organisation*; on a
user-owned repository the built-in `GITHUB_TOKEN` cannot be granted one. So
enforcing it would mean giving up the automatic version bump, or giving the job
a personal access token or deploy key held as a secret and added as a bypass
actor. Until then the pull request flow is a convention — CI still runs on every
one, and promote refuses to release if master has commits develop doesn't.
