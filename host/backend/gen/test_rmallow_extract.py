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

    def compile_layout(self, fields):
        with tempfile.TemporaryDirectory() as root:
            inc = pathlib.Path(root, rmallow_extract.INC)
            inc.mkdir(parents=True)
            header = """
                #include <stdint.h>
                typedef uint32_t NvHandle, NvU32;
                typedef uint8_t NvBool;
                typedef struct {
            """ + fields + "} NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS;"
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
