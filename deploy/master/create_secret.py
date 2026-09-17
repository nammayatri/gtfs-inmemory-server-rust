#!/usr/bin/env python3
"""Create the gtfs-editor-gims-secrets k8s secret for master. Prints no values.

GTFS_EDITOR_SECRETS_DHALL = { master_db_user, master_db_pass, totp_key }

- master_db_user / master_db_pass: application_db_user / application_db_pass
  from the repo .env
- totp_key: 32 random bytes, base64. Created ONCE: every enrolled authenticator
  depends on it, so an existing secret is never overwritten (rotating the key
  means every user re-enrols - do that deliberately with --replace).

The manifest goes to `kubectl apply -f -` on stdin; nothing is written to disk.
"""
import base64
import json
import os
import secrets
import subprocess
import sys

from dotenv import dotenv_values

NAME, NS = "gtfs-editor-gims-secrets", "nandi"


def dhall_text(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\u0024") + '"'


def main():
    replace = "--replace" in sys.argv
    exists = subprocess.run(["kubectl", "-n", NS, "get", "secret", NAME], capture_output=True).returncode == 0
    if exists and not replace:
        print(f"{NS}/{NAME} already exists - left unchanged (pass --replace to rotate)")
        return 0
    env = dotenv_values(os.path.join(os.path.dirname(__file__), "..", "..", ".env")) or dotenv_values(".env")
    user, password = env.get("application_db_user"), env.get("application_db_pass")
    if not user or not password:
        sys.exit("application_db_user / application_db_pass missing from .env")
    record = "{ master_db_user = %s, master_db_pass = %s, totp_key = %s }" % (
        dhall_text(user), dhall_text(password), dhall_text(base64.b64encode(secrets.token_bytes(32)).decode()))
    manifest = {
        "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
        "metadata": {"name": NAME, "namespace": NS},
        "data": {"GTFS_EDITOR_SECRETS_DHALL": base64.b64encode(record.encode()).decode()},
    }
    r = subprocess.run(["kubectl", "apply", "-f", "-"], input=json.dumps(manifest), text=True, capture_output=True)
    print(r.stdout.strip() or r.stderr.strip())
    return r.returncode


if __name__ == "__main__":
    sys.exit(main())
