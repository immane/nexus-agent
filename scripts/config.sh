#!/bin/sh
set -eu

cat <<'EOF'
█▄ █ █▀▀ ▀▄▀ █ █ █▀▀   ▄▀█ █▀▀ █▀▀ █▄ █ ▀█▀
█ ▀█ ██▄ █ █ █▄█ ▄▄█ ▄ █▀█ █▄█ ██▄ █ ▀█  █
EOF

usage() {
    cat <<'EOF'
Usage: nexus-config provider|model

Interactively create or update a provider or model in the Nexus user config.
Credentials can be stored privately or referenced by environment-variable name.
EOF
}

if [ "${1-}" = "--help" ] || [ "$#" -eq 0 ]; then
    usage
    exit 0
fi
if [ "$#" -ne 1 ] || { [ "$1" != provider ] && [ "$1" != model ]; }; then
    usage >&2
    exit 2
fi
command -v python3 >/dev/null 2>&1 || { echo "python3 is required" >&2; exit 1; }
case "$0" in
    */*) script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
         if [ -x "$script_dir/nexus-server" ]; then PATH="$script_dir:$PATH"; export PATH; fi ;;
esac
command -v nexus-server >/dev/null 2>&1 || { echo "nexus-server is required on PATH for config validation" >&2; exit 1; }

exec python3 -c 'exec(compile(open("/dev/fd/3").read(), "nexus-config", "exec"))' "$1" 3<<'PY'
import json
import getpass
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

kind = sys.argv[1]
path = os.environ.get("NEXUS_CONFIG")
if not path:
    base = os.environ.get("XDG_CONFIG_HOME")
    if not base:
        home = os.environ.get("HOME")
        if not home:
            print("nexus-config: HOME or XDG_CONFIG_HOME is required", file=sys.stderr)
            sys.exit(1)
        base = str(Path(home) / ".config")
    path = str(Path(base) / "nexus-agent" / "config.json")
target = Path(path)
credentials_base = os.environ.get("XDG_CONFIG_HOME") or (str(Path(os.environ["HOME"]) / ".config") if os.environ.get("HOME") else None)
credentials_path = Path(credentials_base) / "nexus-agent" / "credentials.json" if credentials_base else None
pending_secret = None
previous_credentials = None
secret_action = None

def prompt(label, default=None):
    suffix = f" [{default}]" if default not in (None, "") else ""
    value = input(f"{label}{suffix}: ").strip()
    return value if value else ("" if default is None else str(default))

def optional_uint(label, old=None):
    raw = prompt(label + " (enter - to clear)", "" if old is None else old)
    if raw == "-":
        return None
    if not raw:
        return None
    if not raw.isdecimal() or not 1 <= int(raw) <= 2**32 - 1:
        raise ValueError(f"{label} must be an integer from 1 to 4294967295")
    return int(raw)

def validate(path):
    result = subprocess.run(["nexus-server", "--config", str(path), "--check-config"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        raise ValueError("Nexus rejected the configuration: " + result.stderr.strip())

def choose_id(label, entries, example):
    print(f"Existing {label} IDs:")
    if not entries:
        print("  (none)")
    for number, entry in enumerate(entries, 1):
        print(f"  {number}. {entry['id']}")
    print("Enter a number or existing ID to edit, or a new ID to create.")
    value = prompt(f"{label.capitalize()} ID (e.g. {example})")
    if value.isascii() and value.isdecimal() and 1 <= int(value) <= len(entries):
        return entries[int(value) - 1]["id"]
    return value

def choose_default_model(provider, models, previous, token):
    names = list(dict.fromkeys(m["name"] for m in models if m["provider"] == provider["id"]))
    endpoint = provider.get("endpoint")
    if endpoint and token:
        protocol = provider["protocol"]
        path = "/v1/models" if protocol == "anthropic_messages" else "/models"
        url = endpoint.rstrip("/") + path
        headers = ({"x-api-key": token, "anthropic-version": "2023-06-01"}
                   if protocol == "anthropic_messages" else {"Authorization": "Bearer " + token})
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, req, fp, code, msg, headers, newurl):
                return None
        try:
            print("Loading remote models from the configured provider...")
            request = urllib.request.Request(url, headers=headers)
            with urllib.request.build_opener(NoRedirect).open(request, timeout=5) as response:
                data = response.read(262145)
            if len(data) > 262144:
                raise ValueError("model list is too large")
            result = json.loads(data)
            if not isinstance(result, dict) or not isinstance(result.get("data"), list):
                raise ValueError("model list has an unsupported format")
            for item in result["data"]:
                name = item.get("id") if isinstance(item, dict) else None
                if isinstance(name, str) and name and len(name.encode()) <= 128 and name not in names:
                    names.append(name)
                if len(names) >= 100:
                    break
        except (OSError, ValueError, urllib.error.URLError):
            print("Remote model list unavailable; use a local model or enter a name manually.")
    else:
        print("Remote model list unavailable (endpoint or credential is missing).")
    if previous and previous not in names:
        names.insert(0, previous)
    print("Available default models:")
    for number, name in enumerate(names, 1):
        print(f"  {number}. {json.dumps(name, ensure_ascii=True)}")
    if not names:
        print("  (none; enter a model name manually)")
    value = prompt("Default model (number or name)", previous)
    if value.isascii() and value.isdecimal() and 1 <= int(value) <= len(names):
        return names[int(value) - 1]
    return value

try:
    if target.is_symlink():
        raise ValueError("refusing to replace a symbolic-link config path")
    if target.exists():
        raw = target.read_text(encoding="utf-8")
        if len(raw.encode("utf-8")) > 65536:
            raise ValueError("configuration exceeds the 65536-byte limit")
        validate(target)
        config = json.loads(raw)
    else:
        raw = None
        config = {"revision": 2, "providers": [], "models": [], "favourites": [], "recent": []}
    if not isinstance(config, dict) or config.get("revision") not in (1, 2):
        raise ValueError("configuration must be a revision 1 or 2 JSON object")
    original_revision = config["revision"]
    for field in ("providers", "models"):
        config.setdefault(field, [])
        if not isinstance(config[field], list) or any(not isinstance(x, dict) for x in config[field]):
            raise ValueError(f"configuration field {field} must be an array of objects")

    if kind == "provider":
        providers = config["providers"]
        existing_id = choose_id("provider", providers, "openai")
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", existing_id):
            raise ValueError("provider ID must contain only letters, digits, _ or - (max 64)")
        old = next((item for item in providers if item.get("id") == existing_id), {})
        label = prompt("Display name", old.get("display_name", existing_id))
        adapter = prompt("Adapter (direct/relay)", old.get("adapter", "direct"))
        protocol = prompt("Protocol (chat_completions/openai_responses/anthropic_messages)", old.get("protocol", "chat_completions" if original_revision == 1 else "openai_responses"))
        endpoint = prompt("Endpoint URL (enter - to clear)", old.get("endpoint") or "")
        if endpoint == "-": endpoint = ""
        credential = old.get("credential", {})
        credential_type = prompt("Credential storage (stored/env)", "stored" if "stored" in credential or not credential else "env")
        if credential_type == "env":
            credential_name = prompt("Credential environment-variable name", credential.get("env", ""))
            if not re.fullmatch(r"[A-Z][A-Z0-9_]{0,63}", credential_name): raise ValueError("credential must be an environment-variable name (A-Z, 0-9, _) starting with a letter")
            credential = {"env": credential_name}
            token = os.environ.get(credential_name)
        elif credential_type == "stored":
            if credentials_path is None: raise ValueError("HOME or XDG_CONFIG_HOME is required for stored credentials")
            if credentials_path.is_symlink(): raise ValueError("refusing symbolic-link credentials file")
            if not sys.stdin.isatty(): raise ValueError("a terminal is required for hidden API key input")
            key_id = credential.get("stored", existing_id)
            secret = getpass.getpass("API key (hidden; blank keeps existing, - deletes): ")
            saved = {}
            if credentials_path.exists():
                if credentials_path.stat().st_size > 65536: raise ValueError("credentials file is too large")
                previous_credentials = credentials_path.read_bytes()
                def unique_pairs(pairs):
                    result = {}
                    for key, value in pairs:
                        if key in result: raise ValueError("credentials file has duplicate entries")
                        result[key] = value
                    return result
                saved = json.loads(previous_credentials, object_pairs_hook=unique_pairs)
                if not isinstance(saved, dict) or any(not isinstance(k, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", k) or not isinstance(v, str) or not v for k, v in saved.items()):
                    raise ValueError("credentials file is invalid")
            if secret == "-":
                if key_id not in saved: raise ValueError("there is no stored API key to delete")
                if any(p.get("id") != existing_id and p.get("credential", {}).get("stored") == key_id for p in providers):
                    raise ValueError("stored API key is also referenced by another provider")
                del saved[key_id]
                pending_secret = saved
                secret_action = "deleted"
                token = None
            else:
                token = secret or saved.get(key_id)
                if not token: raise ValueError("API key is required")
                if any(ord(c) < 32 or ord(c) == 127 for c in token): raise ValueError("API key contains control characters")
            if secret and secret != "-":
                saved[key_id] = secret
                pending_secret = saved
                secret_action = "saved"
            credential = {"stored": key_id}
        else:
            raise ValueError("credential storage must be stored or env")
        if adapter not in ("direct", "relay"): raise ValueError("adapter must be direct or relay")
        if protocol not in ("chat_completions", "openai_responses", "anthropic_messages"): raise ValueError("unsupported protocol")
        if endpoint and (len(endpoint.encode()) > 512 or not endpoint.startswith(("http://", "https://")) or "@" in endpoint or any(c.isspace() or ord(c) < 32 for c in endpoint)): raise ValueError("endpoint URL is invalid")
        selected_provider = {"id": existing_id, "protocol": protocol, "endpoint": endpoint,
                             "credential": credential}
        default_model = choose_default_model(selected_provider, config["models"], old.get("default_model", ""), token)
        context = optional_uint("Default model context window tokens", old.get("context_window_tokens"))
        if not label or len(label.encode()) > 128: raise ValueError("display name must be 1-128 bytes")
        if not default_model or len(default_model.encode()) > 128: raise ValueError("default model name must be 1-128 bytes")
        item = {"id": existing_id, "display_name": label, "adapter": adapter, "protocol": protocol,
                "endpoint": endpoint or None, "credential": credential, "default_model": default_model}
        if context is not None: item["context_window_tokens"] = context
        providers[:] = [p for p in providers if p.get("id") != existing_id]
        providers.append(item)
    else:
        providers = config["providers"]
        if not providers: raise ValueError("add a provider first")
        model_id = choose_id("model", config["models"], "gpt-4o")
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", model_id): raise ValueError("model ID must contain only letters, digits, _ or - (max 64)")
        old = next((item for item in config["models"] if item.get("id") == model_id), {})
        print("Available providers:")
        for number, profile in enumerate(providers, 1):
            print(f"  {number}. {profile['id']}")
        provider = prompt("Provider (number or ID)", old.get("provider", providers[0]["id"]))
        if provider.isascii() and provider.isdecimal() and 1 <= int(provider) <= len(providers):
            provider = providers[int(provider) - 1]["id"]
        if not any(p.get("id") == provider for p in providers): raise ValueError("selected provider does not exist")
        name = prompt("Model name sent to provider", old.get("name", ""))
        if not name or len(name.encode()) > 128: raise ValueError("model name must be 1-128 bytes")
        context = optional_uint("Context window tokens", old.get("context_window_tokens"))
        output = optional_uint("Maximum output tokens", old.get("max_output_tokens"))
        item = {**old, "id": model_id, "provider": provider, "name": name}
        item.pop("context_window_tokens", None)
        item.pop("max_output_tokens", None)
        if context is not None: item["context_window_tokens"] = context
        if output is not None: item["max_output_tokens"] = output
        config["models"][:] = [m for m in config["models"] if m.get("id") != model_id]
        config["models"].append(item)

    if len(config["providers"]) > 16 or len(config["models"]) > 64:
        raise ValueError("provider/model limit exceeded")
    if original_revision == 1:
        for provider in config["providers"]:
            provider.setdefault("protocol", "chat_completions")
    config["revision"] = 2
    data = json.dumps(config, ensure_ascii=False, indent=2) + "\n"
    if len(data.encode("utf-8")) > 65536: raise ValueError("result exceeds the 65536-byte config limit")
    if not target.parent.is_dir(): raise ValueError(f"config directory does not exist: {target.parent} (create it first)")
    fd, temporary = tempfile.mkstemp(prefix=".nexus-config-", dir=target.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
        validate(temporary)
        if target.is_symlink() or (target.read_text(encoding="utf-8") if target.exists() else None) != raw:
            raise ValueError("configuration changed while editing; no changes saved")
        if pending_secret is not None:
            credentials_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            secret_data = json.dumps(pending_secret, ensure_ascii=False).encode("utf-8")
            if len(secret_data) > 65536: raise ValueError("credentials file would be too large")
            secret_fd, secret_temp = tempfile.mkstemp(prefix=".nexus-credentials-", dir=credentials_path.parent)
            try:
                os.fchmod(secret_fd, 0o600)
                with os.fdopen(secret_fd, "wb") as out:
                    out.write(secret_data)
                    out.flush()
                    os.fsync(out.fileno())
                if credentials_path.is_symlink(): raise ValueError("refusing symbolic-link credentials file")
                if (credentials_path.read_bytes() if credentials_path.exists() else None) != previous_credentials:
                    raise ValueError("credentials changed while editing; no changes saved")
                os.replace(secret_temp, credentials_path)
            except BaseException:
                try: os.unlink(secret_temp)
                except FileNotFoundError: pass
                raise
        try:
            os.replace(temporary, target)
        except OSError:
            if pending_secret is not None:
                raise ValueError("credential file was updated, but configuration could not be saved") from None
            raise
    except BaseException:
        try: os.unlink(temporary)
        except FileNotFoundError: pass
        raise
    print(f"Updated {kind} in {target}")
    if pending_secret is not None:
        print(f"API key {secret_action} in the private credentials file.")
except (OSError, ValueError, EOFError) as error:
    print(f"nexus-config: {error}", file=sys.stderr)
    sys.exit(1)
PY
