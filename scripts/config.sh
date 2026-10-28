#!/bin/sh
set -eu

cat <<'EOF'
█▄ █ █▀▀ ▀▄▀ █ █ █▀▀   ▄▀█ █▀▀ █▀▀ █▄ █ ▀█▀
█ ▀█ ██▄ █ █ █▄█ ▄▄█ ▄ █▀█ █▄█ ██▄ █ ▀█  █

EOF

usage() {
    cat <<'EOF'
Usage: nexus-config [provider|model]

Interactively create or update a provider or model in the Nexus user config.
Credentials can be stored privately or referenced by environment-variable name.
EOF
}

if [ "${1-}" = "--help" ]; then
    usage
    exit 0
fi
if [ "$#" -gt 1 ] || { [ "$#" -eq 1 ] && [ "$1" != provider ] && [ "$1" != model ]; }; then
    usage >&2
    exit 2
fi
command -v python3 >/dev/null 2>&1 || { echo "python3 is required" >&2; exit 1; }
case "$0" in
    */*) script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
         if [ -x "$script_dir/nexus-server" ]; then PATH="$script_dir:$PATH"; export PATH; fi ;;
esac
command -v nexus-server >/dev/null 2>&1 || { echo "nexus-server is required on PATH for config validation" >&2; exit 1; }

exec python3 -c 'exec(compile(open("/dev/fd/3").read(), "nexus-config", "exec"))' "${1-menu}" 3<<'PY'
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
selected_id = None
discovery_url = None

def load_credentials():
    global previous_credentials
    if credentials_path is None: raise ValueError("HOME or XDG_CONFIG_HOME is required for stored credentials")
    if credentials_path.is_symlink(): raise ValueError("refusing symbolic-link credentials file")
    if not credentials_path.exists(): return {}
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
    return saved

def prompt(label, default=None):
    suffix = f" [{default}]" if default not in (None, "") else ""
    value = input(f"  > {label}{suffix}: ").strip()
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

def table(headers, rows):
    def cell(value):
        text = json.dumps(str(value), ensure_ascii=True)[1:-1]
        return text if len(text) <= 48 else text[:45] + "..."
    rows = [[cell(value) for value in row] for row in rows]
    widths = [min(48, max(len(header), *(len(row[i]) for row in rows))) if rows else len(header)
              for i, header in enumerate(headers)]
    def line(row):
        print("  " + "  |  ".join(value.ljust(widths[i]) for i, value in enumerate(row)))
    line(headers)
    print("  " + "--+--".join("-" * width for width in widths))
    for row in rows:
        line(row)
    if not rows:
        print("  (none)")
    print()

def choose_id(label, entries, example):
    if selected_id is not None:
        return selected_id
    print(f"Existing {label} IDs:")
    table(("#", "ID"), [(number, entry["id"]) for number, entry in enumerate(entries, 1)])
    print("Enter a number or existing ID to edit, or a new ID to create.")
    value = prompt(f"{label.capitalize()} ID (e.g. {example})")
    if value.isascii() and value.isdecimal() and 1 <= int(value) <= len(entries):
        return entries[int(value) - 1]["id"]
    return value

def choose_default_model(provider, models, previous, token):
    names = list(dict.fromkeys(m["name"] for m in models if m["provider"] == provider["id"]))
    endpoint = provider.get("endpoint")
    is_go = bool(endpoint and endpoint.rstrip("/") == "https://opencode.ai/inference/go/openai/v1"
                 and provider["protocol"] == "chat_completions")
    if endpoint and (token or is_go):
        protocol = provider["protocol"]
        path = "/v1/models" if protocol == "anthropic_messages" else "/models"
        url = discovery_url or ("https://opencode.ai/inference/go/v1/models"
               if endpoint.rstrip("/") == "https://opencode.ai/inference/go/openai/v1" and protocol == "chat_completions"
               else endpoint.rstrip("/") + path)
        headers = ({} if is_go else {"x-api-key": token, "anthropic-version": "2023-06-01"}
                   if protocol == "anthropic_messages" else {"Authorization": "Bearer " + token})
        headers["User-Agent"] = "nexus-agent-config/0.2"
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
        except urllib.error.HTTPError as error:
            print(f"Remote model list unavailable (HTTP {error.code}); use a local model or enter a name manually.")
        except (OSError, ValueError, urllib.error.URLError):
            print("Remote model list unavailable (connection or response error); use a local model or enter a name manually.")
    else:
        print("Remote model list unavailable (endpoint or credential is missing).")
    if previous and previous not in names:
        names.insert(0, previous)
    print("Available default models:")
    table(("#", "Model"), [(number, name) for number, name in enumerate(names, 1)])
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

    menu = kind == "menu"
    preset = None
    advanced = True
    if menu:
        print("CONFIGURATION\n")
        print("Providers:")
        table(("#", "ID", "Name", "Protocol", "Endpoint", "Default model"),
              [(n, p["id"], p.get("display_name", ""), p.get("protocol", ""),
                "configured" if p.get("endpoint") else "none", p.get("default_model", ""))
               for n, p in enumerate(config["providers"], 1)])
        print("Models:")
        table(("#", "ID", "Provider", "Name"),
              [(n, m["id"], m["provider"], m["name"]) for n, m in enumerate(config["models"], 1)])
        print("MANAGE\n")
        print("  Providers                 Models")
        print("  [1] Add                   [4] Add")
        print("  [2] Edit                  [5] Edit")
        print("  [3] Delete                [6] Delete")
        print("\n  [7] Exit\n")
        action = prompt("Select action", "7")
        if action == "7": sys.exit(0)
        if action not in ("1", "2", "3", "4", "5", "6"): raise ValueError("invalid menu choice")
        kind = "provider" if action in ("1", "2", "3") else "model"
        action_name = {"1": "ADD PROVIDER", "2": "EDIT PROVIDER", "3": "DELETE PROVIDER",
                       "4": "ADD MODEL", "5": "EDIT MODEL", "6": "DELETE MODEL"}[action]
        print(f"\n{action_name}\n")
        entries = config[kind + "s"]
        if action in ("2", "3", "5", "6"):
            if not entries: raise ValueError(f"no {kind}s to select")
            choice = prompt(f"Select {kind} number")
            if not choice.isascii() or not choice.isdecimal() or not 1 <= int(choice) <= len(entries): raise ValueError("invalid selection")
            selected_id = entries[int(choice) - 1]["id"]
            print(f"\n  Selected: {selected_id}\n")
        if action == "2":
            advanced = prompt("Edit provider connection and credentials (yes/no)", "no").lower() == "yes"
        if action in ("3", "6"):
            removed = {m["id"] for m in config["models"] if m["provider"] == selected_id} if kind == "provider" else {selected_id}
            print("Models to delete: " + (", ".join(sorted(removed)) or "(none)"))
            for field in ("favourites", "recent"):
                print(f"{field.capitalize()} to remove: " + (", ".join(x for x in config.get(field, []) if x in removed) or "(none)"))
            if prompt("Confirm deletion (yes/no)", "no").lower() != "yes":
                print("Cancelled; no changes saved.")
                sys.exit(0)
            if kind == "provider":
                provider = next(p for p in entries if p["id"] == selected_id)
                key_id = provider.get("credential", {}).get("stored")
                if key_id:
                    shared = any(p["id"] != selected_id and p.get("credential", {}).get("stored") == key_id for p in entries)
                    if shared:
                        print("Stored credential is shared and will be retained.")
                    elif prompt("Also delete the stored credential (yes/no)", "no").lower() == "yes":
                        saved = load_credentials()
                        if key_id in saved:
                            del saved[key_id]
                            pending_secret = saved
                            secret_action = "deleted"
                entries[:] = [p for p in entries if p["id"] != selected_id]
            config["models"][:] = [m for m in config["models"] if m["id"] not in removed]
            for field in ("favourites", "recent"):
                if field in config: config[field] = [x for x in config[field] if x not in removed]
            kind = "deletion"
        elif action == "1":
            print("  Presets                   Other services")
            print("  [1] DeepSeek              [5] OpenAI")
            print("  [2] OpenCode Go           [6] Anthropic")
            print("  [3] Ollama (local)        [7] OpenRouter")
            print("\n  [4] Custom endpoint\n")
            choice = prompt("Provider preset", "4")
            presets = {"1": ("deepseek", "DeepSeek", "https://api.deepseek.com", "chat_completions"),
                       "2": ("opencode-go", "OpenCode Go", "https://opencode.ai/inference/go/openai/v1", "chat_completions"),
                       "3": ("ollama", "Ollama", "http://127.0.0.1:11434/v1", "chat_completions"),
                       "5": ("openai", "OpenAI", "https://api.openai.com/v1", "chat_completions"),
                       "6": ("anthropic", "Anthropic", "https://api.anthropic.com", "anthropic_messages"),
                       "7": ("openrouter", "OpenRouter", "https://openrouter.ai/api/v1", "chat_completions")}
            if choice not in ("1", "2", "3", "4", "5", "6", "7"): raise ValueError("invalid preset")
            preset = presets.get(choice)
            print(f"\n  Using: {preset[1] if preset else 'Custom endpoint'}")
            print("  Press Enter to keep defaults. Advanced settings are optional.\n")
            if preset:
                selected_id = prompt("Provider ID", preset[0])
                if any(p["id"] == selected_id for p in entries): raise ValueError("provider ID already exists")
                advanced = prompt("Advanced settings (yes/no)", "no").lower() == "yes"
                if choice == "2": discovery_url = "https://opencode.ai/inference/go/v1/models"
        if action == "4":
            selected_id = prompt("New model ID")
            if any(m["id"] == selected_id for m in entries): raise ValueError("model ID already exists")

    if kind == "provider":
        providers = config["providers"]
        existing_id = choose_id("provider", providers, "openai")
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", existing_id):
            raise ValueError("provider ID must contain only letters, digits, _ or - (max 64)")
        if menu and action == "1" and any(p.get("id") == existing_id for p in providers):
            raise ValueError("provider ID already exists")
        old = next((item for item in providers if item.get("id") == existing_id), {})
        label = prompt("Display name", old.get("display_name", preset[1] if preset else existing_id)) if advanced else old.get("display_name", preset[1] if preset else existing_id)
        adapter = prompt("Adapter (direct/relay)", old.get("adapter", "direct")) if advanced else old.get("adapter", "direct")
        protocol = prompt("Protocol (chat_completions/openai_responses/anthropic_messages)", old.get("protocol", preset[3] if preset else ("chat_completions" if original_revision == 1 else "openai_responses"))) if advanced else old.get("protocol", preset[3] if preset else "chat_completions")
        endpoint = prompt("Endpoint URL (enter - to clear)", old.get("endpoint") or (preset[2] if preset else "")) if advanced else old.get("endpoint") or (preset[2] if preset else "")
        if discovery_url and (endpoint != preset[2] or protocol != "chat_completions"): discovery_url = None
        if endpoint == "-": endpoint = ""
        credential = old.get("credential", {})
        local_ollama = bool(preset and preset[0] == "ollama" and endpoint == preset[2])
        credential_type = "existing" if old and not advanced else ("local" if local_ollama else prompt("Credential storage (stored/env)", "stored" if "stored" in credential or not credential else "env"))
        if credential_type == "existing":
            token = (os.environ.get(credential["env"]) if "env" in credential
                     else load_credentials().get(credential["stored"]))
        elif credential_type == "local":
            print("Local Ollama uses the nonsecret stored credential 'ollama' because the runtime requires a credential.")
            saved = load_credentials()
            key_id = existing_id
            if key_id in saved and saved[key_id] != "ollama": raise ValueError("stored credential ID already exists; choose another provider ID")
            saved[key_id] = "ollama"
            pending_secret, secret_action, token = saved, "saved", "ollama"
            credential = {"stored": key_id}
        elif credential_type == "env":
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
            saved = load_credentials()
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
        context = optional_uint("Default model context window tokens", old.get("context_window_tokens")) if advanced else old.get("context_window_tokens")
        if not label or len(label.encode()) > 128: raise ValueError("display name must be 1-128 bytes")
        if not default_model or len(default_model.encode()) > 128: raise ValueError("default model name must be 1-128 bytes")
        item = {**old, "id": existing_id, "display_name": label, "adapter": adapter, "protocol": protocol,
                "endpoint": endpoint or None, "credential": credential, "default_model": default_model}
        if context is not None: item["context_window_tokens"] = context
        providers[:] = [p for p in providers if p.get("id") != existing_id]
        providers.append(item)
        if context is None: item.pop("context_window_tokens", None)
        if menu and not any(m["provider"] == existing_id and m["name"] == default_model for m in config["models"]):
            suggested = re.sub(r"[^A-Za-z0-9_-]", "-", default_model)[:64].strip("-") or "model"
            model_id = prompt("Model ID for TUI selection", suggested)
            if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", model_id) or any(m["id"] == model_id for m in config["models"]): raise ValueError("model ID is invalid or already exists")
            config["models"].append({"id": model_id, "provider": existing_id, "name": default_model})
    elif kind == "model":
        providers = config["providers"]
        if not providers: raise ValueError("add a provider first")
        model_id = choose_id("model", config["models"], "gpt-4o")
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", model_id): raise ValueError("model ID must contain only letters, digits, _ or - (max 64)")
        old = next((item for item in config["models"] if item.get("id") == model_id), {})
        print("Available providers:")
        table(("#", "ID"), [(number, profile["id"]) for number, profile in enumerate(providers, 1)])
        provider = prompt("Provider (number or ID)", old.get("provider", providers[0]["id"]))
        if provider.isascii() and provider.isdecimal() and 1 <= int(provider) <= len(providers):
            provider = providers[int(provider) - 1]["id"]
        if not any(p.get("id") == provider for p in providers): raise ValueError("selected provider does not exist")
        name = prompt("Model name sent to provider", old.get("name", ""))
        if not name or len(name.encode()) > 128: raise ValueError("model name must be 1-128 bytes")
        model_advanced = not menu or prompt("Edit model token limits (yes/no)", "no").lower() == "yes"
        context = optional_uint("Context window tokens", old.get("context_window_tokens")) if model_advanced else old.get("context_window_tokens")
        output = optional_uint("Maximum output tokens", old.get("max_output_tokens")) if model_advanced else old.get("max_output_tokens")
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
