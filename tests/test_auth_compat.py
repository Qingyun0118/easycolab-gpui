import io
import json
import os
import stat
import tempfile
import threading
import types
import unittest
import urllib.parse
import urllib.request
from contextlib import redirect_stdout
from unittest import mock

import colab_bridge


class AuthCompatTest(unittest.TestCase):
    def test_private_write_replaces_existing_public_token(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "token.json")
            with open(path, "w", encoding="utf-8") as stream:
                stream.write("old token")
            os.chmod(path, 0o644)
            colab_bridge._write_private_text(path, "new token")
            with open(path, encoding="utf-8") as stream:
                self.assertEqual(stream.read(), "new token")
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o600)
            self.assertEqual(os.listdir(tmp), ["token.json"])

    def test_profile_cache_stores_digest_instead_of_refresh_token_suffix(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "profile.json")
            refresh_token = "private-refresh-token-never-written-to-profile"
            credentials = types.SimpleNamespace(refresh_token=refresh_token, token="access-token")
            response = mock.Mock(status_code=200)
            response.json.return_value = {"email": "demo@example.com"}
            with (
                mock.patch.object(colab_bridge, "ACCOUNT_PROFILE_CACHE_PATH", path),
                mock.patch.object(colab_bridge, "_load_credentials", return_value=credentials),
                mock.patch.object(colab_bridge.requests, "get", return_value=response) as request,
            ):
                self.assertEqual(colab_bridge._get_account_email(), "demo@example.com")
                self.assertEqual(colab_bridge._get_account_email(), "demo@example.com")
                request.assert_called_once()
            with open(path, encoding="utf-8") as stream:
                cached = stream.read()
            self.assertNotIn(refresh_token[-20:], cached)
            self.assertEqual(len(json.loads(cached)["fp"]), 64)

    def _sample_token_payload(self, client_id="1234567890-custom.apps.googleusercontent.com"):
        return {
            "token": "ya29.valid_access_token",
            "refresh_token": "1//valid_refresh_token",
            "token_uri": "https://oauth2.googleapis.com/token",
            "client_id": client_id,
            "client_secret": "test-secret",
            "scopes": list(colab_bridge.PUBLIC_SCOPES),
            "expiry": "2099-01-01T00:00:00Z",
        }

    def test_cmd_auth_url_outputs_google_oauth_url(self):
        with tempfile.TemporaryDirectory() as tmp:
            pkce_file = os.path.join(tmp, "pkce_verifier.txt")
            buf = io.StringIO()
            with (
                mock.patch.object(colab_bridge, "PKCE_VERIFIER_PATH", pkce_file),
                redirect_stdout(buf),
            ):
                colab_bridge.cmd_auth_url()
            payload = json.loads(buf.getvalue().strip())
            self.assertTrue(payload["ok"])
            self.assertIn("https://accounts.google.com/o/oauth2/auth", payload["auth_url"])
            self.assertTrue(os.path.exists(pkce_file))

    def test_has_valid_token_populates_state_client_without_deleting_token_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            tok_file = os.path.join(tmp, "token.json")
            with open(tok_file, "w", encoding="utf-8") as f:
                json.dump(self._sample_token_payload(), f)

            colab_bridge.state._client = None
            with (
                mock.patch.object(colab_bridge, "TOKEN_CONFIG_PATH", tok_file),
                mock.patch.object(colab_bridge, "_keyring_target", return_value=None),
                mock.patch("colab_cli.common.get_credentials", side_effect=AssertionError("must not be called")),
            ):
                self.assertTrue(colab_bridge._has_valid_token())
                self.assertIsNotNone(colab_bridge.state._client)
                self.assertTrue(os.path.exists(tok_file))
            colab_bridge.state._client = None

    def test_has_valid_token_falls_back_to_keyring_and_mirrors_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            tok_file = os.path.join(tmp, "nested", "token.json")
            fake_kr = mock.MagicMock()
            fake_kr.get_password.return_value = json.dumps(self._sample_token_payload())

            colab_bridge.state._client = None
            with (
                mock.patch.object(colab_bridge, "TOKEN_CONFIG_PATH", tok_file),
                mock.patch.object(
                    colab_bridge,
                    "_keyring_target",
                    return_value=(fake_kr, "google-colab-cli", "oauth_token"),
                ),
            ):
                self.assertTrue(colab_bridge._has_valid_token())
                fake_kr.get_password.assert_called_once_with("google-colab-cli", "oauth_token")
                self.assertTrue(os.path.exists(tok_file))
                self.assertIsNotNone(colab_bridge.state._client)
                if os.name == "posix":
                    self.assertEqual(stat.S_IMODE(os.stat(tok_file).st_mode), 0o600)
            colab_bridge.state._client = None

    def test_cmd_auth_logout_clears_file_caches_and_keyring(self):
        with tempfile.TemporaryDirectory() as tmp:
            tok_file = os.path.join(tmp, "token.json")
            pkce_file = os.path.join(tmp, "pkce.txt")
            prof_file = os.path.join(tmp, "profile.json")
            for p in (tok_file, pkce_file, prof_file):
                with open(p, "w", encoding="utf-8") as f:
                    f.write("{}")

            fake_kr = mock.MagicMock()
            buf = io.StringIO()
            with (
                mock.patch.object(colab_bridge, "TOKEN_CONFIG_PATH", tok_file),
                mock.patch.object(colab_bridge, "PKCE_VERIFIER_PATH", pkce_file),
                mock.patch.object(colab_bridge, "ACCOUNT_PROFILE_CACHE_PATH", prof_file),
                mock.patch.object(
                    colab_bridge,
                    "_keyring_target",
                    return_value=(fake_kr, "google-colab-cli", "oauth_token"),
                ),
                mock.patch.object(colab_bridge, "_clear_local_session_store"),
                redirect_stdout(buf),
            ):
                colab_bridge.cmd_auth_logout()

            payload = json.loads(buf.getvalue().strip())
            self.assertEqual(payload, {"ok": True, "logged_in": False, "email": ""})
            for p in (tok_file, pkce_file, prof_file):
                self.assertFalse(os.path.exists(p))
            fake_kr.delete_password.assert_called_once_with("google-colab-cli", "oauth_token")

    def test_cmd_auth_login_receives_loopback_callback_and_saves_token(self):
        with tempfile.TemporaryDirectory() as tmp:
            tok_file = os.path.join(tmp, "token.json")
            pkce_file = os.path.join(tmp, "pkce.txt")
            prof_file = os.path.join(tmp, "profile.json")
            creds_payload = self._sample_token_payload()
            captured = {}

            class FakeFlow:
                def __init__(self):
                    self.redirect_uri = ""
                    self.code_verifier = "test-verifier"
                    self.credentials = types.SimpleNamespace(
                        to_json=lambda: json.dumps(creds_payload)
                    )

                def authorization_url(self, **kwargs):
                    captured["redirect_uri"] = self.redirect_uri
                    captured["kwargs"] = kwargs
                    url = f"https://accounts.google.com/o/oauth2/auth?redirect_uri={urllib.parse.quote(self.redirect_uri, safe='')}&state=state-123"
                    return url, "state-123"

                def fetch_token(self, **kwargs):
                    captured["fetch_kwargs"] = kwargs

            class CaptureStdout(io.StringIO):
                def __init__(self):
                    super().__init__()
                    self.url_ready = threading.Event()

                def write(self, s):
                    n = super().write(s)
                    if "AUTH_URL:" in self.getvalue():
                        self.url_ready.set()
                    return n

            buf = CaptureStdout()
            done = threading.Event()

            def run_login():
                with (
                    mock.patch.object(colab_bridge, "TOKEN_CONFIG_PATH", tok_file),
                    mock.patch.object(colab_bridge, "PKCE_VERIFIER_PATH", pkce_file),
                    mock.patch.object(colab_bridge, "ACCOUNT_PROFILE_CACHE_PATH", prof_file),
                    mock.patch.object(colab_bridge, "_keyring_target", return_value=None),
                    mock.patch.object(colab_bridge, "_clear_local_session_store"),
                    mock.patch.object(
                        colab_bridge.InstalledAppFlow,
                        "from_client_config",
                        return_value=FakeFlow(),
                    ),
                    mock.patch.object(
                        colab_bridge,
                        "cmd_auth_status",
                        side_effect=lambda: print(json.dumps({"ok": True, "logged_in": True, "email": "user@example.com"})),
                    ),
                    redirect_stdout(buf),
                ):
                    colab_bridge.cmd_auth_login(timeout_seconds=10, open_browser=False)
                done.set()

            thread = threading.Thread(target=run_login, daemon=True)
            thread.start()
            self.assertTrue(buf.url_ready.wait(timeout=5))
            redirect_uri = captured["redirect_uri"]
            self.assertTrue(redirect_uri.startswith("http://127.0.0.1:"))

            # Wrong state should return 400 and keep server waiting
            bad_req = urllib.request.Request(f"{redirect_uri}?state=wrong&code=bad")
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(bad_req, timeout=3)
            self.assertEqual(ctx.exception.code, 400)

            # Valid state & code completes login
            with urllib.request.urlopen(f"{redirect_uri}?state=state-123&code=good-auth-code", timeout=5) as resp:
                self.assertEqual(resp.status, 200)
                html = resp.read().decode("utf-8")
                self.assertIn("Authorization received", html)
                self.assertIn("EasyColab GPUI", html)
                self.assertIn("prefers-color-scheme", html)

            self.assertTrue(done.wait(timeout=5))
            self.assertEqual(captured["fetch_kwargs"]["code"], "good-auth-code")
            self.assertEqual(captured["kwargs"]["prompt"], "select_account consent")
            self.assertTrue(os.path.exists(tok_file))
            lines = [line for line in buf.getvalue().splitlines() if line.strip()]
            self.assertTrue(lines[0].startswith("AUTH_URL:https://accounts.google.com/"))
            final_payload = json.loads(lines[-1])
            self.assertTrue(final_payload["logged_in"])
            self.assertEqual(final_payload["email"], "user@example.com")

    def test_cmd_auth_login_preserves_existing_token_on_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            tok_file = os.path.join(tmp, "token.json")
            with open(tok_file, "w", encoding="utf-8") as f:
                json.dump(self._sample_token_payload(), f)
            captured = {}

            class FakeFlow:
                def __init__(self):
                    self.redirect_uri = ""

                def authorization_url(self, **kwargs):
                    captured["redirect_uri"] = self.redirect_uri
                    return "https://accounts.google.com/o/oauth2/auth", "state-xyz"

            class CaptureStdout(io.StringIO):
                def __init__(self):
                    super().__init__()
                    self.url_ready = threading.Event()

                def write(self, s):
                    n = super().write(s)
                    if "AUTH_URL:" in self.getvalue():
                        self.url_ready.set()
                    return n

            buf = CaptureStdout()
            done = threading.Event()

            def run_login():
                with (
                    mock.patch.object(colab_bridge, "TOKEN_CONFIG_PATH", tok_file),
                    mock.patch.object(
                        colab_bridge.InstalledAppFlow,
                        "from_client_config",
                        return_value=FakeFlow(),
                    ),
                    redirect_stdout(buf),
                ):
                    colab_bridge.cmd_auth_login(timeout_seconds=10, open_browser=False)
                done.set()

            thread = threading.Thread(target=run_login, daemon=True)
            thread.start()
            self.assertTrue(buf.url_ready.wait(timeout=5))
            with urllib.request.urlopen(f"{captured['redirect_uri']}?state=state-xyz&error=access_denied", timeout=5) as resp:
                self.assertEqual(resp.status, 200)
            self.assertTrue(done.wait(timeout=5))
            # Previous token must remain intact
            self.assertTrue(os.path.exists(tok_file))
            final_payload = json.loads(buf.getvalue().splitlines()[-1])
            self.assertFalse(final_payload["ok"])
            self.assertEqual(final_payload["error"], "access_denied")

    def test_sanitize_remote_subdir_and_cmd_upload_skips_ignored_dirs(self):
        self.assertEqual(colab_bridge._sanitize_remote_subdir("/content/my_app/"), "my_app")
        self.assertEqual(colab_bridge._sanitize_remote_subdir(""), "project")
        with self.assertRaises(ValueError):
            colab_bridge._sanitize_remote_subdir("../escape")

        with tempfile.TemporaryDirectory() as tmp:
            proj = os.path.join(tmp, "proj")
            os.makedirs(os.path.join(proj, ".git"), exist_ok=True)
            os.makedirs(os.path.join(proj, ".venv"), exist_ok=True)
            os.makedirs(os.path.join(proj, "src"), exist_ok=True)
            with open(os.path.join(proj, ".git", "config"), "w", encoding="utf-8") as f:
                f.write("secret")
            with open(os.path.join(proj, ".venv", "pyvenv.cfg"), "w", encoding="utf-8") as f:
                f.write("venv")
            with open(os.path.join(proj, "src", "train.py"), "w", encoding="utf-8") as f:
                f.write("print('ok')")

            uploaded_members = []
            fake_session = types.SimpleNamespace(
                name="easycolab",
                url="https://proxy.invalid",
                token="tok",
                kernel_id="k1",
                session_id="s1",
            )

            class FakeContents:
                def __init__(self, _session):
                    pass

                def upload(self, local_archive, remote_rel):
                    import tarfile

                    self.remote_rel = remote_rel
                    with tarfile.open(local_archive, "r:gz") as tf:
                        uploaded_members.extend(tf.getnames())

            class FakeRuntime:
                def __init__(self, *args, **kwargs):
                    pass

                def execute_code(self, code, **kwargs):
                    self.code = code
                    return [{"text": "UPLOAD_DONE\n"}]

                def stop(self):
                    pass

            buf = io.StringIO()
            with (
                mock.patch.object(colab_bridge, "_has_valid_token", return_value=True),
                mock.patch.object(colab_bridge, "_resolve_active_session", return_value=fake_session),
                mock.patch.object(colab_bridge, "ContentsClient", FakeContents),
                mock.patch.object(colab_bridge, "ColabRuntime", FakeRuntime),
                redirect_stdout(buf),
            ):
                colab_bridge.cmd_upload(proj, "/content/my_app")

            self.assertEqual(uploaded_members, ["src/train.py"])
            out_lines = [ln for ln in buf.getvalue().splitlines() if ln.strip()]
            progress_lines = [ln for ln in out_lines if ln.startswith("WAKE_PROGRESS:")]
            self.assertGreaterEqual(len(progress_lines), 4)
            self.assertTrue(any("正在打包 (1/1) src/train.py" in ln.replace("\\", "/") for ln in progress_lines))
            self.assertTrue(any(ln.startswith("WAKE_PROGRESS:1.000|") for ln in progress_lines))
            payload = json.loads(out_lines[-1])
            self.assertTrue(payload["ok"])
            self.assertEqual(payload["remote_path"], "/content/my_app")
            self.assertEqual(payload["files"], 1)

    def test_cmd_metrics_collects_vram_ram_disk_from_kernel(self):
        fake_session = types.SimpleNamespace(
            name="easycolab",
            accelerator="G4",
            url="https://proxy.invalid",
            token="tok",
            kernel_id="k1",
            session_id="s1",
        )

        class FakeRuntime:
            def __init__(self, *args, **kwargs):
                pass

            def execute_code(self, code, **kwargs):
                self.code = code
                sample = {
                    "ok": True,
                    "gpu_name": "NVIDIA RTX PRO 6000",
                    "vram_total_gb": 96.0,
                    "vram_used_gb": 4.2,
                    "vram_free_gb": 91.8,
                    "ram_total_gb": 176.0,
                    "ram_used_gb": 10.5,
                    "ram_free_gb": 165.5,
                    "disk_total_gb": 235.0,
                    "disk_used_gb": 30.0,
                    "disk_free_gb": 205.0,
                    "shm_total_gb": 80.0,
                    "shm_used_gb": 0.5,
                    "shm_free_gb": 79.5,
                }
                return [{"text": "METRICS_JSON:" + json.dumps(sample) + "\n"}]

            def stop(self):
                pass

        buf = io.StringIO()
        with (
            mock.patch.object(colab_bridge, "_has_valid_token", return_value=True),
            mock.patch.object(colab_bridge, "_resolve_active_session", return_value=fake_session),
            mock.patch.object(colab_bridge, "ColabRuntime", FakeRuntime),
            redirect_stdout(buf),
        ):
            colab_bridge.cmd_metrics()

        payload = json.loads(buf.getvalue().splitlines()[-1])
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["gpu_name"], "NVIDIA RTX PRO 6000")
        self.assertEqual(payload["vram_total_gb"], 96.0)
        self.assertEqual(payload["ram_total_gb"], 176.0)
        self.assertEqual(payload["disk_total_gb"], 235.0)
        self.assertEqual(payload["shm_total_gb"], 80.0)

    def test_cmd_upload_drive_verifies_real_mount_before_uploading(self):
        self.assertEqual(
            colab_bridge._sanitize_remote_subdir("/content/drive/MyDrive/my_proj/", target="drive"),
            "my_proj",
        )
        with tempfile.TemporaryDirectory() as tmp:
            src_file = os.path.join(tmp, "train.py")
            with open(src_file, "w", encoding="utf-8") as f:
                f.write("print('drive')")

            fake_session = types.SimpleNamespace(
                name="easycolab",
                url="https://proxy.invalid",
                token="tok",
                kernel_id="k1",
                session_id="s1",
            )

            # 1) Unmounted Drive: must abort BEFORE ContentsClient.upload is called
            upload_calls = []

            class SpyContents:
                def __init__(self, _session):
                    pass

                def upload(self, local_path, remote_rel):
                    upload_calls.append(remote_rel)

            class UnmountedRuntime:
                def __init__(self, *args, **kwargs):
                    pass

                def execute_code(self, code, **kwargs):
                    return [{"text": "DRIVE_MOUNTED:0\n"}]

                def stop(self):
                    pass

            buf = io.StringIO()
            with (
                mock.patch.object(colab_bridge, "_has_valid_token", return_value=True),
                mock.patch.object(colab_bridge, "_resolve_active_session", return_value=fake_session),
                mock.patch.object(colab_bridge, "_build_rclone_drive_conf", return_value=""),
                mock.patch.object(colab_bridge, "ContentsClient", SpyContents),
                mock.patch.object(colab_bridge, "ColabRuntime", UnmountedRuntime),
                redirect_stdout(buf),
            ):
                colab_bridge.cmd_upload(src_file, "my_proj", target="drive")

            self.assertEqual(upload_calls, [])
            err_payload = json.loads(buf.getvalue().splitlines()[-1])
            self.assertFalse(err_payload["ok"])
            self.assertIn("尚未挂载", err_payload["error"])

            # 2) Mounted Drive: succeeds and extracts into /content/drive/MyDrive/my_proj
            executed_codes = []

            class MountedRuntime:
                def __init__(self, *args, **kwargs):
                    pass

                def execute_code(self, code, **kwargs):
                    executed_codes.append(code)
                    if "DRIVE_MOUNTED:" in code:
                        return [{"text": "DRIVE_MOUNTED:1\n"}]
                    return [{"text": "UPLOAD_DONE\n"}]

                def stop(self):
                    pass

            buf2 = io.StringIO()
            with (
                mock.patch.object(colab_bridge, "_has_valid_token", return_value=True),
                mock.patch.object(colab_bridge, "_resolve_active_session", return_value=fake_session),
                mock.patch.object(colab_bridge, "_build_rclone_drive_conf", return_value=""),
                mock.patch.object(colab_bridge, "ContentsClient", SpyContents),
                mock.patch.object(colab_bridge, "ColabRuntime", MountedRuntime),
                redirect_stdout(buf2),
            ):
                colab_bridge.cmd_upload(src_file, "my_proj", target="drive")

            self.assertEqual(len(upload_calls), 1)
            self.assertTrue(any("os.sync()" in c for c in executed_codes))
            ok_payload = json.loads(buf2.getvalue().splitlines()[-1])
            self.assertTrue(ok_payload["ok"])
            self.assertEqual(ok_payload["remote_path"], "/content/drive/MyDrive/my_proj")
            self.assertEqual(ok_payload["files"], 1)

    def test_runtime_auth_nginx_origin_check_uses_dynamic_variable_comparison(self):
        import runtime_auth

        cfg = runtime_auth.nginx_config("/tmp/test-auth")
        # Nginx `if ($http_origin = https://$http_host)` does NOT expand $http_host on RHS
        # and breaks all same-origin WebSocket & POST requests with 403!
        self.assertNotIn("if ($http_origin = https://$http_host)", cfg)
        self.assertIn('set $origin_pair "$http_origin https://$http_host";', cfg)
        self.assertIn(r'if ($origin_pair ~ "^([^ ]+) \1$")', cfg)
        self.assertIn("gzip on;", cfg)
        self.assertIn("gzip_proxied any;", cfg)
        self.assertIn("application/json", cfg)
        self.assertIn("application/javascript", cfg)


if __name__ == "__main__":
    unittest.main()
