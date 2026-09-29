#!/usr/bin/env python3
"""The webhook of the merge-set App, in AWS Lambda.

GitHub sends every event of the repositories where the merge-set App is
installed to the function URL of this function. The function checks the
signature of each delivery with the webhook secret, and hands the event to the
relay of merge_set.py (`relay`), which starts the Merge set workflow of peppy
for an event that can change a set, and answers a box that a user ticked. It
answers GitHub with what the relay did, which the App settings show next to
each delivery, and writes the same line to its log.

The private key of the App is in AWS KMS, which signs the JSON Web Token of
the App and never gives out the key. With that token, the function makes the
installation tokens that the relay asks for, each for one scope, and keeps
each one until EXPIRY_MARGIN_SECONDS before it expires. The webhook secret is
in AWS Secrets Manager, which the function reads again when a signature does
not match, so it takes a new secret without a restart.

The stack aws/peppy-ci-pipelines of Peppy-bot/infraops holds the AWS side of
the function: the function, its role, the App key, the webhook secret, and the
role the Merge set webhook workflow (.github/workflows/merge-set-webhook.yml)
deploys the archive of the `package` subcommand with. It fixes the contract of
this file: HANDLER, the variables of the function (the *_VARIABLE names) and
WEBHOOK_SECRET_KEY. A runbook of infraops imports the App key and puts the
webhook secret.

The decisions are functions of their inputs, tested in test_webhook.py: the
clock, KMS and GitHub are parameters. Standard library only, and the boto3
of the Lambda runtime, which only the function's own start imports.
"""

from __future__ import annotations

import argparse
import base64
import functools
import hashlib
import hmac
import io
import json
import os
import sys
import time
import zipfile
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import merge_set
import resolve

# The contract with aws/peppy-ci-pipelines of Peppy-bot/infraops, which sets
# the function up: the entry point of its runtime, the variables it sets on
# the function, and the one key of the JSON document of the webhook secret.
HANDLER = "webhook.lambda_handler"
APP_CLIENT_ID_VARIABLE = "APP_CLIENT_ID"
APP_SLUG_VARIABLE = "APP_SLUG"
APP_KEY_ID_VARIABLE = "APP_KEY_ID"
WEBHOOK_SECRET_ARN_VARIABLE = "WEBHOOK_SECRET_ARN"
WEBHOOK_SECRET_KEY = "webhook_secret"


class WebhookError(Exception):
    """A delivery that the function refuses, with the HTTP status of the
    refusal."""

    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status = status


# The delivery -----------------------------------------------------------------


@dataclass(frozen=True)
class Delivery:
    """A webhook delivery whose signature proves that GitHub sent it."""

    event_name: str
    delivery_id: str
    payload: Mapping

    @property
    def action(self) -> str | None:
        return self.payload.get("action")


SIGNATURE_PREFIX = "sha256="


def signature_of(secret: bytes, body: bytes) -> str:
    """The `X-Hub-Signature-256` header of a delivery of `body`."""
    return SIGNATURE_PREFIX + hmac.new(secret, body, hashlib.sha256).hexdigest()


def request_body(request: Mapping) -> bytes:
    """The body of a function URL request, as GitHub sent it."""
    body = request.get("body") or ""
    if request.get("isBase64Encoded"):
        return base64.b64decode(body)
    return body.encode()


def signed_delivery(request: Mapping, secret: bytes) -> Delivery:
    """The delivery of a function URL request, once its signature proves that
    GitHub sent it with the webhook secret. The function URL gives the names
    of the headers in lower case. An empty secret checks nothing, since anyone
    can sign with it, so it refuses every delivery."""
    method = ((request.get("requestContext") or {}).get("http") or {}).get("method")
    if method != "POST":
        raise WebhookError(405, "a webhook delivery is a POST request")
    if not secret:
        raise WebhookError(
            503,
            "the webhook secret is empty; the runbook of aws/peppy-ci-pipelines in "
            "Peppy-bot/infraops puts it",
        )
    headers = request.get("headers") or {}
    body = request_body(request)
    signature = headers.get("x-hub-signature-256", "")
    if not hmac.compare_digest(signature.encode(), signature_of(secret, body).encode()):
        raise WebhookError(
            401, "the signature of the delivery does not match the webhook secret"
        )
    event_name = headers.get("x-github-event")
    if not event_name:
        raise WebhookError(400, "the delivery names no event")
    try:
        payload = json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise WebhookError(400, f"the payload is not JSON: {error}") from error
    if not isinstance(payload, dict):
        raise WebhookError(400, "the payload is not a JSON object")
    return Delivery(event_name, headers.get("x-github-delivery", ""), payload)


def installation_id(payload: Mapping) -> int:
    """The installation of the App that the event comes from."""
    installation = (payload.get("installation") or {}).get("id")
    if not isinstance(installation, int):
        raise merge_set.MergeSetError("the event names no installation of the App")
    return installation


# The webhook secret -----------------------------------------------------------

# The least time between two reads of the webhook secret that a delivery with
# another signature starts. A flood of unsigned requests makes at most one
# read a minute for each instance of the function.
SECRET_REFRESH_SECONDS = 60


def parse_webhook_secret(secret_string: str) -> bytes:
    """The webhook secret in the JSON document of Secrets Manager,
    {"webhook_secret": "<secret>"}. It is empty until a human puts it."""
    try:
        document = json.loads(secret_string)
    except json.JSONDecodeError as error:
        raise merge_set.MergeSetError(
            f"the webhook secret is not a JSON document: {error}"
        ) from error
    value = document.get(WEBHOOK_SECRET_KEY) if isinstance(document, dict) else None
    if not isinstance(value, str):
        raise merge_set.MergeSetError(
            f"the webhook secret is not a JSON object with a string `{WEBHOOK_SECRET_KEY}`"
        )
    return value.encode()


class WebhookSecret:
    """The webhook secret, read at the first delivery, and read again when a
    delivery's signature does not match it or it is empty, at most once every
    SECRET_REFRESH_SECONDS: the function takes a new secret within a minute of
    its change in Secrets Manager."""

    def __init__(self, read: Callable[[], str], clock: Callable[[], float]):
        self.read = read
        self.clock = clock
        self.value: bytes | None = None
        self.read_at: float | None = None

    def load(self) -> bytes:
        self.value = parse_webhook_secret(self.read())
        self.read_at = self.clock()
        return self.value

    def current(self) -> bytes:
        return self.load() if self.value is None else self.value

    def refreshed(self) -> bool:
        """Read the secret again, unless the last read is less than
        SECRET_REFRESH_SECONDS old; whether it read."""
        if (
            self.read_at is not None
            and self.clock() - self.read_at < SECRET_REFRESH_SECONDS
        ):
            return False
        self.load()
        return True


# The statuses of a delivery that a new webhook secret could let through.
SECRET_REFUSALS = frozenset({401, 503})


# The tokens -------------------------------------------------------------------


def base64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def compact(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode()


JWT_HEADER = base64url(compact({"alg": "RS256", "typ": "JWT"}))

# GitHub refuses a JWT issued in the future, or that expires more than 10
# minutes after the time it is sent: the issue time is set back a minute for
# a clock of AWS ahead of the one of GitHub, and the expiry is 9 minutes out.
JWT_BACKDATE_SECONDS = 60
JWT_LIFETIME_SECONDS = 9 * 60

# A token is used until this long before it expires, so that no request of
# the relay starts with a token about to expire, even with the clocks of AWS
# and GitHub apart.
EXPIRY_MARGIN_SECONDS = 5 * 60


def app_jwt(client_id: str, now: int, sign: Callable[[bytes], bytes]) -> str:
    """The JSON Web Token of the App at `now`, signed RS256 by `sign`."""
    claims = base64url(
        compact(
            {
                "iat": now - JWT_BACKDATE_SECONDS,
                "exp": now + JWT_LIFETIME_SECONDS,
                "iss": client_id,
            }
        )
    )
    signing_input = f"{JWT_HEADER}.{claims}"
    return f"{signing_input}.{base64url(sign(signing_input.encode()))}"


def kms_signer(kms: object, key_id: str) -> Callable[[bytes], bytes]:
    """RS256 with the key of the App in KMS: RSASSA-PKCS1-v1_5 with SHA-256
    over the message, which KMS hashes itself."""

    def sign(message: bytes) -> bytes:
        return kms.sign(
            KeyId=key_id,
            Message=message,
            MessageType="RAW",
            SigningAlgorithm="RSASSA_PKCS1_V1_5_SHA_256",
        )["Signature"]

    return sign


@dataclass(frozen=True)
class Token:
    value: str
    expires_at: float


def parse_installation_token(response: object) -> Token:
    """The token of the answer to POST /app/installations/{id}/access_tokens."""
    try:
        return Token(
            value=response["token"],
            expires_at=datetime.fromisoformat(response["expires_at"]).timestamp(),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise merge_set.MergeSetError(
            f"GitHub gave no installation token: {json.dumps(response)}"
        ) from error


class AppTokens:
    """The tokens of the App: its JWT, and the installation tokens it makes
    with it, each kept until EXPIRY_MARGIN_SECONDS before it expires."""

    def __init__(
        self,
        client_id: str,
        sign: Callable[[bytes], bytes],
        clock: Callable[[], float],
    ):
        self.client_id = client_id
        self.sign = sign
        self.clock = clock
        self.jwt: Token | None = None
        self.installation_tokens: dict[tuple[int, merge_set.TokenScope], Token] = {}

    def is_usable(self, token: Token | None) -> bool:
        return (
            token is not None
            and self.clock() < token.expires_at - EXPIRY_MARGIN_SECONDS
        )

    def app_jwt(self) -> str:
        if not self.is_usable(self.jwt):
            now = int(self.clock())
            self.jwt = Token(
                app_jwt(self.client_id, now, self.sign), now + JWT_LIFETIME_SECONDS
            )
        return self.jwt.value

    def installation_token(self, installation: int, scope: merge_set.TokenScope) -> str:
        key = (installation, scope)
        token = self.installation_tokens.get(key)
        if not self.is_usable(token):
            token = parse_installation_token(
                merge_set.GitHubApi(self.app_jwt()).request(
                    "POST",
                    f"/app/installations/{installation}/access_tokens",
                    body={
                        "repositories": list(scope.repositories),
                        "permissions": dict(scope.permissions),
                    },
                )
            )
            self.installation_tokens[key] = token
        return token.value


# The function -----------------------------------------------------------------


def response(status: int, text: str) -> dict:
    """A function URL response."""
    return {
        "statusCode": status,
        "headers": {"Content-Type": "text/plain; charset=utf-8"},
        "body": text,
    }


class Webhook:
    """The function: it checks each delivery, and hands its event to the
    relay."""

    def __init__(self, secret: WebhookSecret, bot_login: str, tokens: AppTokens):
        self.secret = secret
        self.bot_login = bot_login
        self.tokens = tokens

    def verified_delivery(self, request: Mapping) -> Delivery:
        """The delivery of the request, checked with the webhook secret, and
        with a new read of it if the one the function holds refuses it."""
        try:
            return signed_delivery(request, self.secret.current())
        except WebhookError as error:
            if error.status not in SECRET_REFUSALS or not self.secret.refreshed():
                raise
        return signed_delivery(request, self.secret.current())

    def handle(self, request: Mapping) -> dict:
        try:
            delivery = self.verified_delivery(request)
        except (WebhookError, merge_set.MergeSetError) as error:
            status = error.status if isinstance(error, WebhookError) else 500
            print(f"Refused a request: HTTP {status}: {error}")
            return response(status, str(error))
        label = f"{delivery.delivery_id} {delivery.event_name}"
        if delivery.action:
            label = f"{label}.{delivery.action}"
        try:
            text = self.relay(delivery)
        except (merge_set.MergeSetError, resolve.ResolveError) as error:
            print(f"{label}: failed: {error}")
            return response(500, str(error))
        print(f"{label}: {text}")
        return response(200, text)

    def relay(self, delivery: Delivery) -> str:
        # GitHub sends a ping when the webhook of the App is set up.
        if delivery.event_name == "ping":
            return "pong"

        def api_for(scope: merge_set.TokenScope) -> merge_set.GitHubApi:
            return merge_set.GitHubApi(
                self.tokens.installation_token(installation_id(delivery.payload), scope)
            )

        return merge_set.relay(
            delivery.event_name, delivery.payload, self.bot_login, api_for
        )


def webhook_of_environment(
    environment: Mapping[str, str], client: Callable[[str], object]
) -> Webhook:
    """The function as aws/peppy-ci-pipelines sets it up. `client` makes the
    AWS client of a service."""
    secrets = client("secretsmanager")
    secret_arn = required(environment, WEBHOOK_SECRET_ARN_VARIABLE)

    def read_secret() -> str:
        return secrets.get_secret_value(SecretId=secret_arn)["SecretString"]

    return Webhook(
        secret=WebhookSecret(read_secret, time.time),
        bot_login=f"{required(environment, APP_SLUG_VARIABLE)}[bot]",
        tokens=AppTokens(
            client_id=required(environment, APP_CLIENT_ID_VARIABLE),
            sign=kms_signer(client("kms"), required(environment, APP_KEY_ID_VARIABLE)),
            clock=time.time,
        ),
    )


def required(environment: Mapping[str, str], name: str) -> str:
    value = environment.get(name, "")
    if not value:
        raise merge_set.MergeSetError(
            f"{name} is not set; aws/peppy-ci-pipelines of Peppy-bot/infraops sets "
            "it on the function"
        )
    return value


@functools.cache
def lambda_webhook() -> Webhook:
    """The function, made once for each instance of it, at its first
    delivery: it keeps its webhook secret and its tokens."""
    import boto3

    return webhook_of_environment(os.environ, boto3.client)


def lambda_handler(event: Mapping, context: object) -> dict:
    return lambda_webhook().handle(event)


# The package ------------------------------------------------------------------

# The code of the function: this file, merge_set.py and the resolve.py it
# imports. They go at the root of the archive, which the runtime puts on the
# path, so merge_set.py finds resolve.py next to it.
PACKAGE_FILES = (
    Path(__file__).resolve(),
    Path(merge_set.__file__).resolve(),
    Path(resolve.__file__).resolve(),
)

# The date of every file of the archive, so that the same code gives the same
# archive.
PACKAGE_DATE = (1980, 1, 1, 0, 0, 0)


def package(files: Sequence[Path]) -> bytes:
    """The archive of the function."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for path in files:
            info = zipfile.ZipInfo(path.name, date_time=PACKAGE_DATE)
            info.external_attr = 0o644 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, path.read_bytes())
    return buffer.getvalue()


def parse_arguments(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="The webhook of the merge-set App.")
    commands = parser.add_subparsers(
        dest="command", metavar="subcommand", required=True
    )
    package_command = commands.add_parser(
        "package", help="Write the archive of the function."
    )
    package_command.add_argument("archive", type=Path)
    return parser.parse_args(argv)


def main(argv: Sequence[str]) -> int:
    arguments = parse_arguments(argv)
    match arguments.command:
        case "package":
            arguments.archive.write_bytes(package(PACKAGE_FILES))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
