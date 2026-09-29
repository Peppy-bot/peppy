#!/usr/bin/env python3
"""Tests for the webhook of the merge-set App.

The clock is a FakeClock that a test moves, KMS is a stand-in that records
what it signs, and GitHub is a stand-in of GitHubApi.request that answers from
a table and records each call. Nothing touches the network or AWS. The last
cases hold the stack, the deploy workflow and the import script to the names
the function uses.
"""

import base64
import hashlib
import hmac
import io
import json
import subprocess
import sys
import tempfile
import unittest
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

import merge_set
import resolve
import webhook
from merge_set import MergeSetError
from test_merge_set import (
    BOT,
    RELAY_RUN_URL,
    WORKFLOWS,
    pull_request_event,
    workflow_lines,
)

MERGE_SET = Path(__file__).resolve().parent
STACK = MERGE_SET / "webhook-stack.yml"
IMPORT_SCRIPT = MERGE_SET / "import-app-key.sh"
DEPLOY_WORKFLOW = "merge-set-webhook.yml"

SECRET = b"webhook-secret"
CLIENT_ID = "Iv23client"
INSTALLATION = 5
NOW = 1_800_000_000
SYNC_SCOPE = merge_set.SYNC_START_SCOPE
NODES_HUB_SCOPE = merge_set.pull_request_scope(
    merge_set.REPOSITORIES_BY_NAME["nodes-hub"]
)


def function_url_request(
    event_name, payload, secret=SECRET, method="POST", signature=None, base64_body=False
):
    """A function URL request of a delivery of `payload`."""
    body = json.dumps(payload).encode() if not isinstance(payload, bytes) else payload
    headers = {
        "x-github-event": event_name,
        "x-github-delivery": "d-1",
        "x-hub-signature-256": (
            webhook.signature_of(secret, body) if signature is None else signature
        ),
    }
    return {
        "requestContext": {"http": {"method": method}},
        "headers": headers,
        "body": base64.b64encode(body).decode() if base64_body else body.decode(),
        "isBase64Encoded": base64_body,
    }


def decoded(part):
    return json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))


class FakeClock:
    def __init__(self, now=NOW):
        self.now = now

    def __call__(self):
        return self.now


class FakeSigner:
    """KMS as the function sees it: records each message it signs."""

    def __init__(self):
        self.messages = []

    def __call__(self, message):
        self.messages.append(message)
        return b"signature-" + str(len(self.messages)).encode()


class FakeGitHub:
    """GitHubApi.request of every GitHubApi: answers from `answers`, keyed by
    (method, path), and records each call as (token, method, path, body)."""

    def __init__(self, answers):
        self.answers = answers
        self.calls = []

    def __enter__(self):
        fake = self

        def request(api, method, path, query=None, body=None):
            fake.calls.append((api.token, method, path, body))
            answer = fake.answers[(method, path)]
            if isinstance(answer, Exception):
                raise answer
            return answer

        self.patch = patch.object(merge_set.GitHubApi, "request", request)
        self.patch.__enter__()
        return self

    def __exit__(self, *details):
        self.patch.__exit__(*details)

    def token_requests(self):
        return [call for call in self.calls if call[2].startswith("/app/")]


def token_answer(name, expires_at=NOW + 3600):
    """GitHub's answer to a request for an installation token."""
    expiry = datetime.fromtimestamp(expires_at, timezone.utc)
    return {"token": name, "expires_at": expiry.strftime("%Y-%m-%dT%H:%M:%SZ")}


TOKENS_PATH = f"/app/installations/{INSTALLATION}/access_tokens"
DISPATCH_PATH = "/repos/Peppy-bot/peppy/actions/workflows/merge-set.yml/dispatches"


class SignedDeliveries(unittest.TestCase):
    def test_a_delivery_signed_with_the_secret_is_read(self):
        payload = {"action": "opened", "number": 7}
        for base64_body in (False, True):
            with self.subTest(base64_body=base64_body):
                delivery = webhook.signed_delivery(
                    function_url_request(
                        "pull_request", payload, base64_body=base64_body
                    ),
                    SECRET,
                )
                self.assertEqual(
                    delivery, webhook.Delivery("pull_request", "d-1", payload)
                )
                self.assertEqual(delivery.action, "opened")

    def test_the_signature_is_the_hmac_of_the_body_with_the_secret(self):
        body = b'{"zen": "Keep it logically awesome."}'
        self.assertEqual(
            webhook.signature_of(SECRET, body),
            "sha256=" + hmac.new(SECRET, body, hashlib.sha256).hexdigest(),
        )

    def test_a_delivery_without_the_signature_of_the_secret_is_refused(self):
        for case, request in (
            ("another secret", function_url_request("ping", {}, secret=b"other")),
            ("no signature", function_url_request("ping", {}, signature="")),
            ("another body", {**function_url_request("ping", {}), "body": '{"a": 1}'}),
            ("not an ASCII signature", function_url_request("ping", {}, signature="é")),
        ):
            with self.subTest(case=case):
                with self.assertRaises(webhook.WebhookError) as refused:
                    webhook.signed_delivery(request, SECRET)
                self.assertEqual(refused.exception.status, 401)

    def test_a_request_that_is_no_delivery_is_refused(self):
        no_event = function_url_request("ping", {})
        del no_event["headers"]["x-github-event"]
        for case, request, status in (
            ("a GET", function_url_request("ping", {}, method="GET"), 405),
            ("no event", no_event, 400),
            ("not JSON", function_url_request("ping", b"{"), 400),
            ("not an object", function_url_request("ping", b"[]"), 400),
            ("not UTF-8", function_url_request("ping", b"\xff", base64_body=True), 400),
        ):
            with self.subTest(case=case):
                with self.assertRaises(webhook.WebhookError) as refused:
                    webhook.signed_delivery(request, SECRET)
                self.assertEqual(refused.exception.status, status)

    def test_the_installation_of_an_event_is_read(self):
        self.assertEqual(webhook.installation_id({"installation": {"id": 5}}), 5)
        for payload in ({}, {"installation": None}, {"installation": {"id": "5"}}):
            with self.subTest(payload=payload), self.assertRaises(MergeSetError):
                webhook.installation_id(payload)


class AppJwts(unittest.TestCase):
    def test_the_jwt_names_the_app_and_lives_nine_minutes_from_a_minute_ago(self):
        sign = FakeSigner()
        token = webhook.app_jwt(CLIENT_ID, NOW, sign)
        header, claims, signature = token.split(".")
        self.assertEqual(decoded(header), {"alg": "RS256", "typ": "JWT"})
        self.assertEqual(
            decoded(claims), {"iat": NOW - 60, "exp": NOW + 540, "iss": CLIENT_ID}
        )
        self.assertEqual(sign.messages, [f"{header}.{claims}".encode()])
        self.assertEqual(
            base64.urlsafe_b64decode(signature + "=" * (-len(signature) % 4)),
            b"signature-1",
        )

    def test_kms_signs_the_jwt_with_rs256(self):
        class FakeKms:
            def sign(self, **arguments):
                self.arguments = arguments
                return {"Signature": b"kms-signature"}

        kms = FakeKms()
        sign = webhook.kms_signer(kms, "arn:aws:kms:key")
        self.assertEqual(sign(b"header.claims"), b"kms-signature")
        self.assertEqual(
            kms.arguments,
            {
                "KeyId": "arn:aws:kms:key",
                "Message": b"header.claims",
                "MessageType": "RAW",
                "SigningAlgorithm": "RSASSA_PKCS1_V1_5_SHA_256",
            },
        )


class AppTokens(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.sign = FakeSigner()
        self.tokens = webhook.AppTokens(CLIENT_ID, self.sign, self.clock)

    def test_an_installation_token_has_the_scope_it_is_made_for(self):
        with FakeGitHub({("POST", TOKENS_PATH): token_answer("t-1")}) as github:
            self.assertEqual(
                self.tokens.installation_token(INSTALLATION, SYNC_SCOPE), "t-1"
            )
        ((jwt, _, _, body),) = github.calls
        self.assertEqual(jwt, self.tokens.app_jwt())
        self.assertEqual(
            body, {"repositories": ["peppy"], "permissions": {"actions": "write"}}
        )

    def test_a_token_is_kept_until_five_minutes_before_it_expires(self):
        with FakeGitHub({("POST", TOKENS_PATH): token_answer("t-1")}) as github:
            self.tokens.installation_token(INSTALLATION, SYNC_SCOPE)
            self.clock.now = NOW + 3600 - 300 - 1
            self.tokens.installation_token(INSTALLATION, SYNC_SCOPE)
            self.assertEqual(len(github.token_requests()), 1)
            self.clock.now = NOW + 3600 - 300
            self.tokens.installation_token(INSTALLATION, SYNC_SCOPE)
            self.assertEqual(len(github.token_requests()), 2)

    def test_each_scope_and_installation_has_its_own_token(self):
        with FakeGitHub(
            {
                ("POST", TOKENS_PATH): token_answer("t-1"),
                ("POST", "/app/installations/6/access_tokens"): token_answer("t-2"),
            }
        ) as github:
            self.tokens.installation_token(INSTALLATION, SYNC_SCOPE)
            self.tokens.installation_token(INSTALLATION, NODES_HUB_SCOPE)
            self.tokens.installation_token(6, SYNC_SCOPE)
            self.tokens.installation_token(INSTALLATION, NODES_HUB_SCOPE)
        self.assertEqual(
            [body for _, _, _, body in github.calls],
            [
                {"repositories": ["peppy"], "permissions": {"actions": "write"}},
                {
                    "repositories": ["nodes-hub"],
                    "permissions": {"pull_requests": "write"},
                },
                {"repositories": ["peppy"], "permissions": {"actions": "write"}},
            ],
        )

    def test_the_jwt_is_signed_again_five_minutes_before_it_expires(self):
        first = self.tokens.app_jwt()
        self.clock.now = NOW + 540 - 300 - 1
        self.assertEqual(self.tokens.app_jwt(), first)
        self.assertEqual(len(self.sign.messages), 1)
        self.clock.now = NOW + 540 - 300
        second = self.tokens.app_jwt()
        self.assertNotEqual(second, first)
        self.assertEqual(decoded(second.split(".")[1])["iat"], NOW + 240 - 60)

    def test_an_answer_without_a_token_is_refused(self):
        for answer in (None, {}, {"token": "t"}, {"token": "t", "expires_at": "soon"}):
            with self.subTest(answer=answer), self.assertRaises(MergeSetError):
                webhook.parse_installation_token(answer)


class Handling(unittest.TestCase):
    def setUp(self):
        self.webhook = webhook.Webhook(
            SECRET, BOT, webhook.AppTokens(CLIENT_ID, FakeSigner(), FakeClock())
        )

    def handle(self, request, answers=None):
        """The response of the function, its API calls and its log."""
        log = io.StringIO()
        with FakeGitHub(answers or {}) as github, patch("sys.stdout", log):
            result = self.webhook.handle(request)
        return result, github.calls, log.getvalue()

    def test_a_ping_gets_a_pong(self):
        result, calls, log = self.handle(function_url_request("ping", {"zen": "z"}))
        self.assertEqual(result["statusCode"], 200)
        self.assertEqual(result["body"], "pong")
        self.assertEqual(calls, [])
        self.assertEqual(log, "d-1 ping: pong\n")

    def test_an_event_that_can_change_a_set_starts_the_sync(self):
        result, calls, log = self.handle(
            function_url_request("pull_request", pull_request_event("synchronize")),
            {
                ("POST", TOKENS_PATH): token_answer("sync-token"),
                ("POST", DISPATCH_PATH): {"html_url": RELAY_RUN_URL},
            },
        )
        self.assertEqual(result["statusCode"], 200)
        self.assertIn(RELAY_RUN_URL, result["body"])
        self.assertEqual(
            [(token, path) for token, _, path, _ in calls][1:],
            [("sync-token", DISPATCH_PATH)],
        )
        self.assertTrue(log.startswith("d-1 pull_request.synchronize: Started"))

    def test_an_event_that_cannot_change_a_set_calls_nothing(self):
        result, calls, _ = self.handle(
            function_url_request("pull_request", pull_request_event("labeled"))
        )
        self.assertEqual(
            (result["statusCode"], result["body"]),
            (200, "The `pull_request` event cannot change a set."),
        )
        self.assertEqual(calls, [])

    def test_a_failed_relay_is_an_error_of_the_delivery(self):
        refused = merge_set.ApiError("POST dispatches", 422, "No ref found")
        result, _, log = self.handle(
            function_url_request("pull_request", pull_request_event("opened")),
            {
                ("POST", TOKENS_PATH): token_answer("sync-token"),
                ("POST", DISPATCH_PATH): refused,
            },
        )
        self.assertEqual(result["statusCode"], 500)
        self.assertEqual(result["body"], str(refused))
        self.assertIn("d-1 pull_request.opened: failed: ", log)

    def test_an_event_without_its_installation_gets_no_token(self):
        payload = pull_request_event("opened")
        del payload["installation"]
        result, calls, _ = self.handle(function_url_request("pull_request", payload))
        self.assertEqual(result["statusCode"], 500)
        self.assertIn("names no installation", result["body"])
        self.assertEqual(calls, [])

    def test_an_unsigned_request_is_refused_before_it_is_read(self):
        result, calls, log = self.handle(
            function_url_request("ping", b"not JSON", signature="sha256=0")
        )
        self.assertEqual(result["statusCode"], 401)
        self.assertEqual(calls, [])
        self.assertIn("HTTP 401", log)


# The environment that webhook-stack.yml gives the function.
STACK_ENVIRONMENT = {
    "APP_CLIENT_ID": CLIENT_ID,
    "APP_SLUG": "peppy-merge-set",
    "APP_KEY_ID": "arn:aws:kms:key",
    "WEBHOOK_SECRET_ARN": "arn:aws:secretsmanager:secret",
}


class Environment(unittest.TestCase):
    def client(self, service):
        test = self

        class FakeSecretsManager:
            def get_secret_value(self, SecretId):
                test.secret_id = SecretId
                return {"SecretString": "the-secret"}

        class FakeKms:
            def sign(self, **arguments):
                test.key_id = arguments["KeyId"]
                return {"Signature": b"s"}

        return {"secretsmanager": FakeSecretsManager, "kms": FakeKms}[service]()

    def test_the_function_reads_its_secret_and_key_from_the_stack(self):
        function = webhook.webhook_of_environment(STACK_ENVIRONMENT, self.client)
        self.assertEqual(self.secret_id, "arn:aws:secretsmanager:secret")
        self.assertEqual(function.secret, b"the-secret")
        self.assertEqual(function.bot_login, "peppy-merge-set[bot]")
        self.assertEqual(function.tokens.client_id, CLIENT_ID)
        function.tokens.app_jwt()
        self.assertEqual(self.key_id, "arn:aws:kms:key")

    def test_a_function_without_a_variable_of_the_stack_does_not_start(self):
        for name in STACK_ENVIRONMENT:
            environment = {**STACK_ENVIRONMENT, name: ""}
            with self.subTest(name=name), self.assertRaises(MergeSetError):
                webhook.webhook_of_environment(environment, self.client)


class Package(unittest.TestCase):
    def test_the_archive_holds_the_code_of_the_function_at_its_root(self):
        archive = zipfile.ZipFile(io.BytesIO(webhook.package(webhook.PACKAGE_FILES)))
        self.assertEqual(
            archive.namelist(), ["webhook.py", "merge_set.py", "resolve.py"]
        )
        for path in webhook.PACKAGE_FILES:
            with self.subTest(file=path.name):
                self.assertEqual(archive.read(path.name), path.read_bytes())
                self.assertEqual(
                    archive.getinfo(path.name).date_time, webhook.PACKAGE_DATE
                )

    def test_the_same_code_gives_the_same_archive(self):
        self.assertEqual(
            webhook.package(webhook.PACKAGE_FILES),
            webhook.package(webhook.PACKAGE_FILES),
        )

    def test_the_runtime_finds_the_handler_in_the_archive(self):
        # The runtime puts the root of the archive on the path and imports the
        # module of the handler. An isolated interpreter reads nothing of this
        # checkout.
        module, function = webhook.HANDLER.split(".")
        with tempfile.TemporaryDirectory() as directory:
            with zipfile.ZipFile(
                io.BytesIO(webhook.package(webhook.PACKAGE_FILES))
            ) as archive:
                archive.extractall(directory)
            result = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    "-c",
                    (
                        f"import sys; sys.path.insert(0, {directory!r}); "
                        f"import {module}; print(callable({module}.{function}))"
                    ),
                ],
                cwd=directory,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(
            (result.returncode, result.stdout), (0, "True\n"), result.stderr
        )

    def test_the_package_command_writes_the_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "webhook.zip"
            self.assertEqual(webhook.main(["package", str(path)]), 0)
            self.assertEqual(path.read_bytes(), webhook.package(webhook.PACKAGE_FILES))


def stack_lines():
    return [line.strip() for line in STACK.read_text().splitlines()]


class RepositoryFacts(unittest.TestCase):
    def test_the_stack_runs_the_handler_of_this_file_with_its_variables(self):
        lines = stack_lines()
        for line in (
            f"FunctionName: {webhook.FUNCTION_NAME}",
            f"Handler: {webhook.HANDLER}",
            f"LogGroupName: /aws/lambda/{webhook.FUNCTION_NAME}",
            f"{webhook.APP_CLIENT_ID_VARIABLE}: !Ref AppClientId",
            f"{webhook.APP_SLUG_VARIABLE}: !Ref AppSlug",
            f"{webhook.APP_KEY_ID_VARIABLE}: !GetAtt AppKey.Arn",
            f"{webhook.WEBHOOK_SECRET_ARN_VARIABLE}: !Ref WebhookSecret",
        ):
            with self.subTest(line=line):
                self.assertIn(line, lines)

    def test_the_stack_names_the_app_of_the_sync(self):
        lines = stack_lines()
        slug = lines[lines.index("AppSlug:") + 3]
        self.assertEqual(slug, f"Default: {BOT.removesuffix('[bot]')}")

    def test_the_function_answers_within_the_wait_of_github(self):
        # GitHub waits 10 seconds for the answer to a delivery; a delivery
        # that runs longer still ends its work.
        (timeout,) = [line for line in stack_lines() if line.startswith("Timeout:")]
        self.assertGreater(int(timeout.split(":")[1]), 10)

    def test_the_deploy_role_trusts_the_environment_of_the_deploy_workflow(self):
        lines = workflow_lines(DEPLOY_WORKFLOW)
        self.assertIn("environment: merge-set-webhook", lines)
        self.assertIn("id-token: write", lines)
        self.assertIn(
            '"token.actions.githubusercontent.com:sub": '
            f'"repo:{resolve.PEPPY_REPOSITORY}:environment:merge-set-webhook"',
            stack_lines(),
        )

    def test_the_deploy_workflow_deploys_the_package_of_this_file(self):
        lines = workflow_lines(DEPLOY_WORKFLOW)
        self.assertIn(
            "run: python3 .github/merge-set/webhook.py package webhook.zip", lines
        )
        self.assertIn("--zip-file fileb://webhook.zip \\", lines)
        self.assertEqual(
            sum(
                line.startswith(f"--function-name {webhook.FUNCTION_NAME}")
                for line in lines
            ),
            3,
        )

    def test_a_change_to_a_file_of_the_package_deploys_it(self):
        lines = workflow_lines(DEPLOY_WORKFLOW)
        root = MERGE_SET.parents[1]
        for path in webhook.PACKAGE_FILES:
            with self.subTest(file=path.name):
                self.assertIn(f"- {path.relative_to(root)}", lines)
        self.assertIn(f"- .github/workflows/{DEPLOY_WORKFLOW}", lines)

    def test_the_deploy_starts_at_once_and_ends_within_the_limit_of_its_runner(self):
        # An ubuntu-slim job stops after 15 minutes.
        lines = workflow_lines(DEPLOY_WORKFLOW)
        self.assertIn("runs-on: ubuntu-slim", lines)
        (timeout,) = [line for line in lines if line.startswith("timeout-minutes:")]
        self.assertLessEqual(int(timeout.split(":")[1]), 15)

    def test_the_import_script_imports_into_the_key_of_the_stack(self):
        lines = [line.strip() for line in IMPORT_SCRIPT.read_text().splitlines()]
        (stack_name,) = [
            line.removeprefix("STACK_NAME=")
            for line in lines
            if line.startswith("STACK_NAME=")
        ]
        self.assertIn(f"--stack-name {stack_name} \\", STACK.read_text())
        self.assertIn(
            "--query \"Stacks[0].Outputs[?OutputKey=='AppKeyId'].OutputValue\" \\",
            lines,
        )
        self.assertIn("AppKeyId:", stack_lines())

    def test_the_changes_job_runs_these_cases(self):
        text = (WORKFLOWS / "tests.yml").read_text()
        self.assertIn("--start-directory .github/merge-set", text)
        self.assertIn("--pattern 'test_*.py'", text)


if __name__ == "__main__":
    unittest.main()
