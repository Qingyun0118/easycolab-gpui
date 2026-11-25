"""Exercise the cloud output route without importing GPU dependencies."""
import ast
import os
from pathlib import Path
import tempfile
import unittest


class OutputPathTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.output = self.root / "comfy" / "output"
        self.output.mkdir(parents=True)

        class HTTPException(Exception):
            def __init__(self, status_code, detail):
                self.status_code = status_code

        self.error = HTTPException
        source = Path(__file__).resolve().parents[1] / "python" / "app.py"
        tree = ast.parse(source.read_text(encoding="utf-8"))
        route = next(node for node in ast.walk(tree)
                     if isinstance(node, ast.FunctionDef)
                     and node.name == "api_comfyui_output_media")
        route.decorator_list = []
        namespace = {
            "os": os,
            "touch_activity": lambda: None,
            "_resolve_comfyui_persistence_paths": lambda: (
                str(self.output.parent), str(self.root / "persist"), None, None, None
            ),
            "HTTPException": HTTPException,
            "FileResponse": lambda path, **kwargs: path,
        }
        module = ast.fix_missing_locations(ast.Module(body=[route], type_ignores=[]))
        exec(compile(module, str(source), "exec"), namespace)
        self.route = namespace["api_comfyui_output_media"]

    def test_downloads_nested_output(self):
        nested = self.output / "images" / "result.png"
        nested.parent.mkdir()
        nested.write_bytes(b"demo")
        self.assertEqual(self.route("images/result.png"), str(nested.resolve()))

    def test_rejects_prefix_sibling_and_absolute_escape(self):
        outside = self.output.parent / "output-private" / "secret.json"
        outside.parent.mkdir()
        outside.write_text("private", encoding="utf-8")
        for path in ("../output-private/secret.json", str(outside.resolve())):
            with self.subTest(path=path), self.assertRaises(self.error) as caught:
                self.route(path)
            self.assertEqual(caught.exception.status_code, 400)

    def test_rejects_symlink_escape(self):
        outside = self.root / "private.json"
        outside.write_text("private", encoding="utf-8")
        link = self.output / "linked.json"
        try:
            link.symlink_to(outside)
        except OSError:
            self.skipTest("symlinks unavailable for this user")
        with self.assertRaises(self.error) as caught:
            self.route("linked.json")
        self.assertEqual(caught.exception.status_code, 400)
