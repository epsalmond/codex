#!/usr/bin/env python3
"""Report exact failed updater validation to the existing recovery deployment route.

Runs from trusted workflow base code, never from the failed candidate. Payload
context is a bounded hint; the processor independently repeats API verification.
"""

import codecs
import collections
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass

REPOSITORY = "epsalmond/codex"
REPOSITORY_ID = 1365250630
API = "https://api.github.com"
PREPARE_JOB = "Prepare exact upstream merge"
VALIDATION_JOB = (
    "Validate exact candidate before landing / Validate exact upstream merge"
)
IDENTITY_STEP = "Verify the exact candidate branch and commit"
MAX_DOWNLOAD = 16 * 1024 * 1024
MAX_STORAGE = 128 * 1024
MAX_CONTEXT = 12 * 1024
MAX_LINE = 2048
MAX_CONTEXT_LINES = 160
SOURCE_STEPS = {"Check formatting", "Test scoped release behavior"}
SHA = re.compile(r"[0-9a-f]{40}\Z")
TAG = re.compile(r"rust-v[0-9]+\.[0-9]+\.[0-9]+\Z")
INFRA = re.compile(
    r"no space left on device|disk quota exceeded|out of memory|oom.kill|"
    r"failed to download|could not resolve host|temporary failure in name resolution|"
    r"connection (?:reset|refused|timed out)|network is unreachable|"
    r"toolchain .* (?:not installed|is not available)|"
    r"error: (?:could not|failed to) (?:download|fetch)|"
    r"command not found|process didn't exit successfully.*(?:signal: 9|SIGKILL)|"
    r"operation was canceled|job was canceled|exceeded the maximum execution time",
    re.I,
)
RUSTC = re.compile(r"\berror\[E[0-9]{4}\]:")
FORMAT = re.compile(r"\bDiff in .+:\d+:")
TEST = re.compile(
    r"(?:\btest .+ \.{3} FAILED\b|\bFAIL\s+\[.+\]\s+.+|\bthread '.+' panicked at .+:\d+)"
)


class HandoffError(Exception):
    pass


def positive(value, name):
    if type(value) is not int or value <= 0:
        raise HandoffError(f"invalid {name}")
    return value


@dataclass(frozen=True)
class Identity:
    upstream_tag: str
    upstream_sha: str
    fork_sha: str
    candidate_sha: str
    candidate_branch: str
    run_id: int
    run_attempt: int

    def validate(self):
        if not TAG.fullmatch(self.upstream_tag):
            raise HandoffError("invalid stable upstream tag")
        for value in (self.upstream_sha, self.fork_sha, self.candidate_sha):
            if not SHA.fullmatch(value):
                raise HandoffError("invalid full SHA")
        root = f"{self.upstream_tag}/{self.fork_sha[:12]}"
        allowed = {f"fork-upstream-candidate/{root}", f"fork-recovery/{root}"}
        allowed.update(f"fork-recovery/{root}-attempt-{n}" for n in (1, 2))
        if self.candidate_branch not in allowed:
            raise HandoffError("invalid candidate branch")
        positive(self.run_id, "run_id")
        positive(self.run_attempt, "run_attempt")

    @property
    def run_url(self):
        return f"https://github.com/{REPOSITORY}/actions/runs/{self.run_id}"


def sanitize(text):
    text = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", text)
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
    text = re.sub(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]", "", text)
    text = re.sub(r"^\d{4}-\d\d-\d\dT\S+\s*", "", text)
    # Keep diagnostic content from the runner's legacy error annotation, but
    # discard arbitrary workflow command lines before transporting them.
    text = re.sub(r"##\[(?:error|warning|debug|group|endgroup)\]", "", text)
    if re.match(r"\s*::[\w-]+(?:\s[^:]*)?::", text):
        return ""
    text = re.sub(r"https?://[^\s<>\"']+", lambda m: redact_url(m.group()), text)
    text = re.sub(
        r"\b(?:github_pat_[A-Za-z0-9_]+|gh[pousr]_[A-Za-z0-9_]+|"
        r"sk-[A-Za-z0-9_-]+|AKIA[A-Z0-9]{16})\b",
        "[REDACTED]",
        text,
    )
    text = re.sub(
        r"(?i)\b(authorization|token|password|secret|api[_-]?key)\s*[:=]\s*"
        r"(?:bearer\s+)?[^\s,;]+",
        r"\1=[REDACTED]",
        text,
    )
    # Explicit token redaction also covers private token formats unknown here.
    token = os.environ.get("GH_TOKEN", "")
    if token:
        text = text.replace(token, "[REDACTED]")
    return text


def redact_url(value):
    parsed = urllib.parse.urlsplit(value)
    # URLs in diagnostics are data, never fetch targets. Remove all query/fragment
    # values and userinfo so signed download links and embedded credentials stay private.
    host = parsed.netloc.rsplit("@", 1)[-1]
    return urllib.parse.urlunsplit((parsed.scheme, host, parsed.path, "", ""))


def utf8_prefix(value, limit):
    return value.encode("utf-8")[:limit].decode("utf-8", errors="ignore")


def diagnostic(line, step):
    if step == "Check formatting" and FORMAT.search(line):
        return "formatter_diff"
    if step == "Test scoped release behavior":
        if RUSTC.search(line):
            return "rustc_diagnostics"
        if TEST.search(line):
            return "test_failure"
    return None


def collect_log(chunks, failed_step):
    """Stream entire bounded log; keep first diagnostic windows and terminal tail.

    Infrastructure signatures anywhere in the download take precedence. A transport
    ceiling prevents source classification even if a prior compiler line matched.
    """
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    pending = ""
    before = collections.deque(maxlen=2)
    selected = []
    tail = collections.deque()
    tail_bytes = 0
    selected_bytes = 0
    remaining = 0
    reason = None
    infrastructure = False
    truncated = False
    exceeded = False
    downloaded = 0

    def line(value):
        nonlocal \
            tail_bytes, \
            selected_bytes, \
            remaining, \
            reason, \
            infrastructure, \
            truncated
        clean = sanitize(value)
        if not clean:
            return
        infra = bool(INFRA.search(clean))
        bounded = utf8_prefix(clean, MAX_LINE)
        truncated |= bounded != clean
        clean = bounded
        found = diagnostic(clean, failed_step)
        if (
            infra
            and not infrastructure
            or found
            and reason is None
            and not infrastructure
        ):
            selected[:] = list(before)
            selected_bytes = sum(len(x.encode("utf-8")) + 1 for x in selected)
            remaining = 24
        infrastructure |= infra
        reason = reason or found
        if (
            remaining > 0
            and selected_bytes + len(clean.encode("utf-8")) + 1 <= MAX_STORAGE // 2
        ):
            selected.append(clean)
            selected_bytes += len(clean.encode("utf-8")) + 1
            remaining -= 1
        tail.append(clean)
        tail_bytes += len(clean.encode("utf-8")) + 1
        while tail_bytes > MAX_STORAGE // 2:
            tail_bytes -= len(tail.popleft().encode("utf-8")) + 1
            truncated = True
        before.append(clean)

    for chunk in chunks:
        downloaded += len(chunk)
        if downloaded > MAX_DOWNLOAD:
            exceeded = truncated = True
            break
        pending += decoder.decode(chunk).replace("\r", "\n")
        while "\n" in pending or len(pending) > MAX_LINE * 2:
            if "\n" in pending:
                value, pending = pending.split("\n", 1)
            else:
                value, pending = pending[: MAX_LINE * 2], pending[MAX_LINE * 2 :]
                truncated = True
            line(value)
    pending += decoder.decode(b"", final=True)
    if pending and not exceeded:
        line(pending)
    if exceeded:
        result, reason = "infrastructure_failure", "log_download_limit"
    elif infrastructure:
        result, reason = "infrastructure_failure", "infrastructure_diagnostics"
    elif failed_step not in SOURCE_STEPS:
        result, reason = "infrastructure_failure", "setup_or_identity_failure"
    elif reason:
        result = "source_failure"
    else:
        result, reason = "infrastructure_failure", "unclassified"
    head = "\n".join(selected)
    end = "\n".join(tail)
    context = end if not head or head == end else head + "\n[terminal log tail]\n" + end
    context_lines = context.splitlines()
    if len(context_lines) > MAX_CONTEXT_LINES:
        context = "\n".join(
            context_lines[:100] + ["[... log lines omitted ...]"] + context_lines[-59:]
        )
        truncated = True
    if len(context.encode("utf-8")) > MAX_CONTEXT:
        context = (
            utf8_prefix(context, 8 * 1024)
            + "\n[terminal log tail]\n"
            + context.encode("utf-8")[-3800:].decode("utf-8", "ignore")
        )
        truncated = True
    # The byte-window join can duplicate lines; enforce the line limit again.
    context_lines = context.splitlines()
    if len(context_lines) > MAX_CONTEXT_LINES:
        context = "\n".join(
            context_lines[:100] + ["[... log lines omitted ...]"] + context_lines[-59:]
        )
        truncated = True
    return {
        "validation_result": result,
        "failure_reason": reason,
        "failure_context": context,
        "context_truncated": truncated,
    }


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class GitHub:
    def __init__(self, token):
        self.token = token
        self.opener = urllib.request.build_opener(NoRedirect())

    def request(self, path, body=None):
        if not path.startswith(f"/repos/{REPOSITORY}/") and not path.startswith(
            "/repos/openai/codex/git/"
        ):
            raise HandoffError("non-fixed GitHub API path")
        request = urllib.request.Request(
            API + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "Content-Type": "application/json",
            },
        )
        with self.opener.open(request, timeout=30) as response:
            data = response.read(2 * 1024 * 1024 + 1)
            if len(data) > 2 * 1024 * 1024:
                raise HandoffError("API metadata too large")
            return json.loads(data)

    def log_chunks(self, job_id):
        request = urllib.request.Request(
            f"{API}/repos/{REPOSITORY}/actions/jobs/{job_id}/logs",
            headers={"Authorization": f"Bearer {self.token}"},
        )
        try:
            response = self.opener.open(request, timeout=30)
        except urllib.error.HTTPError as error:
            if error.code not in (301, 302, 303, 307, 308):
                raise
            location = error.headers.get("Location", "")
            parsed = urllib.parse.urlsplit(location)
            if (
                parsed.scheme != "https"
                or parsed.username
                or parsed.password
                or not parsed.hostname
            ):
                raise HandoffError("unsafe job log redirect") from None
            # No API credentials on the signed log URL, including further redirects.
            response = urllib.request.urlopen(location, timeout=30)
        started = time.monotonic()
        with response:
            while True:
                if time.monotonic() - started > 60:
                    raise HandoffError("job log download deadline")
                chunk = response.read(64 * 1024)
                if not chunk:
                    break
                yield chunk


def unique(items, predicate, label):
    matches = [item for item in items if predicate(item)]
    if len(matches) != 1:
        raise HandoffError(f"missing or ambiguous {label}")
    return matches[0]


def successful_step(job, name):
    step = unique(job.get("steps", []), lambda item: item.get("name") == name, name)
    return step.get("status") == "completed" and step.get("conclusion") == "success"


def pages(api, path, key=None):
    # Bound metadata work independently of transport; fail closed rather than omit
    # ambiguous jobs or dedupe history after a partial page scan.
    for page in range(1, 101):
        data = api.request(
            f"{path}{'&' if '?' in path else '?'}per_page=100&page={page}"
        )
        items = data[key] if key else data
        if not isinstance(items, list):
            raise HandoffError("invalid API page")
        yield from items
        if len(items) < 100:
            return
    raise HandoffError("API pagination limit")


def verify(api, identity):
    identity.validate()
    prefix = f"/repos/{REPOSITORY}"
    run = api.request(f"{prefix}/actions/runs/{identity.run_id}")
    if (
        run.get("id") != identity.run_id
        or type(run.get("run_attempt")) is not int
        or run.get("run_attempt") != identity.run_attempt
        or run.get("repository", {}).get("full_name") != REPOSITORY
        or type(run.get("repository", {}).get("id")) is not int
        or run["repository"]["id"] != REPOSITORY_ID
        or run.get("event") != "workflow_dispatch"
        or run.get("head_branch") != "main"
        or run.get("head_sha") != identity.fork_sha
        or run.get("path") != ".github/workflows/fork-upstream-update.yml"
        or run.get("html_url") != identity.run_url
    ):
        raise HandoffError("wrong or stale workflow run provenance")
    jobs = list(
        pages(
            api,
            f"{prefix}/actions/runs/{identity.run_id}/attempts/{identity.run_attempt}/jobs",
            "jobs",
        )
    )
    prepare = unique(jobs, lambda job: job.get("name") == PREPARE_JOB, "prepare job")
    validation = unique(
        jobs, lambda job: job.get("name") == VALIDATION_JOB, "validation job"
    )
    job_id = positive(validation.get("id"), "validation_job_id")
    for job in (prepare, validation):
        if (
            job.get("run_id") != identity.run_id
            or type(job.get("run_attempt")) is not int
            or job.get("run_attempt") != identity.run_attempt
            or job.get("head_sha") != identity.fork_sha
        ):
            raise HandoffError("wrong validation/prepare job run binding")
    if validation.get("status") != "completed" or validation.get("conclusion") not in (
        "failure",
        "cancelled",
        "timed_out",
    ):
        raise HandoffError("validation job is not terminal failure")
    failed = [
        step
        for step in validation.get("steps", [])
        if step.get("conclusion") in ("failure", "cancelled", "timed_out")
    ]
    if len(failed) > 1:
        raise HandoffError("ambiguous failed validation step")
    failed_step = failed[0].get("name", "") if failed else "Validation job terminated"
    if not isinstance(failed_step, str) or not failed_step or len(failed_step) > 200:
        raise HandoffError("invalid failed validation step")
    ready = (
        prepare.get("status") == "completed"
        and prepare.get("conclusion") == "success"
        and successful_step(prepare, "Prepare merge candidate")
    )
    exact = successful_step(validation, IDENTITY_STEP)
    main = api.request(f"{prefix}/git/ref/heads/main").get("object", {}).get("sha")
    candidate = (
        api.request(f"{prefix}/git/ref/heads/{identity.candidate_branch}")
        .get("object", {})
        .get("sha")
    )
    if main != identity.fork_sha:
        raise HandoffError("main advanced before validation handoff")
    if candidate != identity.candidate_sha:
        raise HandoffError("candidate branch moved before validation handoff")
    commit = api.request(f"{prefix}/git/commits/{identity.candidate_sha}")
    if commit.get("sha") != identity.candidate_sha or [
        p.get("sha") for p in commit.get("parents", [])
    ] != [identity.fork_sha, identity.upstream_sha]:
        raise HandoffError("candidate does not have exact fork/upstream parents")
    tag = api.request(f"/repos/openai/codex/git/ref/tags/{identity.upstream_tag}").get(
        "object", {}
    )
    for _ in range(4):
        if tag.get("type") != "tag":
            break
        if not SHA.fullmatch(tag.get("sha", "")):
            raise HandoffError("invalid upstream tag object")
        tag = api.request(f"/repos/openai/codex/git/tags/{tag['sha']}").get(
            "object", {}
        )
    if tag.get("type") != "commit" or tag.get("sha") != identity.upstream_sha:
        raise HandoffError("upstream tag moved before validation handoff")
    classification = collect_log(api.log_chunks(job_id), failed_step)
    if not ready or not exact or validation.get("conclusion") != "failure":
        classification.update(
            validation_result="infrastructure_failure",
            failure_reason="setup_or_identity_failure",
        )
    return {
        "schema_version": 3,
        "recovery_kind": "validation_failure",
        **identity.__dict__,
        "run_url": identity.run_url,
        "validation_job_id": job_id,
        "failed_step": failed_step,
        **classification,
    }


def validate_deployment(deployment, identity, payload):
    if (
        type(deployment.get("id")) is not int
        or deployment["id"] <= 0
        or deployment.get("environment") != "upstream-release"
        or deployment.get("task") != "upstream-release"
        or deployment.get("ref") != identity.fork_sha
        or deployment.get("sha") != identity.fork_sha
        or deployment.get("creator", {}).get("login") != "github-actions[bot]"
        or deployment.get("creator", {}).get("id") != 41898282
        or deployment.get("payload") != payload
    ):
        raise HandoffError("wrong deployment provenance")


def handoff(api, identity):
    payload = verify(api, identity)
    prefix = f"/repos/{REPOSITORY}"
    for deployment in pages(api, f"{prefix}/deployments?environment=upstream-release"):
        old = deployment.get("payload", {})
        if (
            isinstance(old, dict)
            and old.get("schema_version") == 3
            and old.get("recovery_kind") == "validation_failure"
            and old.get("candidate_sha") == identity.candidate_sha
            and old.get("run_id") == identity.run_id
            and old.get("run_attempt") == identity.run_attempt
        ):
            validate_deployment(deployment, identity, payload)
            return deployment, payload, True
    body = {
        "ref": identity.fork_sha,
        "environment": "upstream-release",
        "task": "upstream-release",
        "auto_merge": False,
        "required_contexts": [],
        "transient_environment": True,
        "description": f"upstream validation failure for {identity.upstream_tag}",
        "payload": payload,
    }
    deployment = api.request(f"{prefix}/deployments", body)
    validate_deployment(deployment, identity, payload)
    return deployment, payload, False


def main():
    if os.environ.get("GITHUB_REPOSITORY") != REPOSITORY or os.environ.get(
        "GITHUB_REPOSITORY_ID"
    ) != str(REPOSITORY_ID):
        raise HandoffError("wrong workflow repository")
    identity = Identity(
        os.environ["UPSTREAM_TAG"],
        os.environ["UPSTREAM_SHA"],
        os.environ["FORK_SHA"],
        os.environ["CANDIDATE_SHA"],
        os.environ["CANDIDATE_BRANCH"],
        int(os.environ["GITHUB_RUN_ID"]),
        int(os.environ["GITHUB_RUN_ATTEMPT"]),
    )
    api = GitHub(os.environ["GH_TOKEN"])
    deployment, payload, reused = handoff(api, identity)
    deployment_id = deployment["id"]
    # The API URL is a stable authenticated evidence link; do not invent a UI URL.
    url = f"{API}/repos/{REPOSITORY}/deployments/{deployment_id}/statuses"
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(
            f"deployment_id={deployment_id}\ndeployment_url={url}\nvalidation_result={payload['validation_result']}\nfailure_reason={payload['failure_reason']}\n"
        )
    print(
        f"{'reused' if reused else 'created'} validation handoff {deployment_id}: {payload['validation_result']} ({payload['failure_reason']})"
    )
    if reused:
        # Retain the existing stalled/failed handoff diagnostic behavior.
        statuses = list(
            pages(api, f"/repos/{REPOSITORY}/deployments/{deployment_id}/statuses")
        )
        if statuses and max(statuses, key=lambda s: s["id"]).get("state") in (
            "failure",
            "error",
        ):
            raise HandoffError(
                "existing validation recovery handoff failed; inspect deployment statuses"
            )
        if not statuses or max(statuses, key=lambda s: s["id"]).get("state") in (
            "queued",
            "pending",
            "in_progress",
        ):
            import datetime

            latest = max(statuses, key=lambda s: s["id"]) if statuses else deployment
            created = datetime.datetime.fromisoformat(
                latest["created_at"].replace("Z", "+00:00")
            ).timestamp()
            if time.time() - created > 6 * 3600:
                raise HandoffError(
                    "existing validation recovery handoff stalled; inspect processor"
                )


if __name__ == "__main__":
    try:
        main()
    except HandoffError as error:
        print(f"validation failure handoff failed: {error}", file=sys.stderr)
        sys.exit(1)
    except (KeyError, ValueError, TypeError, OSError, urllib.error.URLError):
        # Never print request/redirect URLs, response bodies or credential-bearing
        # exceptions. Public output is compact; job logs remain the full evidence.
        print(
            "validation failure handoff failed: invalid provenance, API/log transport, or failed/stalled existing handoff",
            file=sys.stderr,
        )
        sys.exit(1)
