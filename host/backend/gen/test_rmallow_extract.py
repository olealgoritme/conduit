#!/usr/bin/env python3
import pathlib
import subprocess
import tempfile
import unittest

import rmallow_extract


class ChannelGroupAbiTest(unittest.TestCase):
    prefix = """
        NvHandle hObjectError;
        NvHandle hObjectEccError;
        NvHandle hVASpace;
        NvU32 engineType;
        NvBool bIsCallingContextVgpuPlugin;
    """
    tail = "NvU32 reserved; NvU32 internalFlags;"
    tail_620 = "NvU32 reserved; NvHandle reserved2; NvU32 internalFlags;"
    ctx_620 = "NvHandle hVASpace; NvU32 flags; NvU32 subctxId; NvHandle reserved;"

    def compile_layout(self, fields, ctx=None):
        with tempfile.TemporaryDirectory() as root:
            inc = pathlib.Path(root, rmallow_extract.INC)
            inc.mkdir(parents=True)
            header = """
                #include <stdint.h>
                typedef uint32_t NvHandle, NvU32;
                typedef uint8_t NvBool;
                typedef struct {
            """ + fields + "} NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS;"
            if ctx is not None:
                header += f"\ntypedef struct {{ {ctx} }} NV_CTXSHARE_ALLOCATION_PARAMETERS;\n"
            (inc / "nvos.h").write_text(header)
            source = '#include <stddef.h>\n#include "nvos.h"\n'
            source += rmallow_extract.channel_group_abi(root)
            return subprocess.run(
                ["cc", "-std=c11", "-I", str(inc), "-fsyntax-only", "-x", "c", "-"],
                input=source, capture_output=True, text=True,
            )

    def test_known_layouts(self):
        for tail in ("", self.tail):
            with self.subTest(tail=tail):
                result = self.compile_layout(self.prefix + tail)
                self.assertEqual(result.returncode, 0, result.stderr)
        result = self.compile_layout(self.prefix + self.tail_620, self.ctx_620)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.compile_layout(self.prefix + self.tail,
                                     self.ctx_620.removesuffix(" NvHandle reserved;"))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_620_context_share_layout_is_checked(self):
        moved = "NvHandle hVASpace; NvU32 subctxId; NvU32 flags; NvHandle reserved;"
        for tail in (self.tail, self.tail_620):
            result = self.compile_layout(self.prefix + tail, moved)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("context-share flags layout", result.stderr)
        with self.assertRaisesRegex(SystemExit, "unsupported context-share fields"):
            self.compile_layout(self.prefix + self.tail_620)

    def test_moved_fields_with_the_same_total_size(self):
        prefix = self.prefix.replace("NvHandle hVASpace;", "NvHandle hObjectError;")
        prefix = prefix.replace("NvHandle hObjectError;", "NvHandle hVASpace;", 1)
        for tail in ("", self.tail):
            with self.subTest(tail=tail):
                result = self.compile_layout(prefix + tail)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("channel-group hVASpace layout", result.stderr)

    def test_new_fields_require_a_known_layout(self):
        for fields in (self.prefix + self.tail + "NvU32 extra;",
                       self.prefix + "NvBool extra;",
                       self.prefix + "NvBool extra;" + self.tail):
            with self.subTest(fields=fields):
                with self.assertRaisesRegex(SystemExit, "unsupported channel-group fields"):
                    self.compile_layout(fields)


if __name__ == "__main__":
    unittest.main()
