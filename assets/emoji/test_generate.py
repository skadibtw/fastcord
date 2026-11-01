"""Independent decoder and migration-equivalence checks for the pinned index."""
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import sys
import unittest

sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("emoji_generator", HERE / "generate.py")
generator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generator)


class CompactIndex(unittest.TestCase):
    def test_all_fields_and_lookup_match_the_pre_compaction_table(self):
        data = generator.render()
        self.assertEqual(data, (HERE / "emoji-index.bin").read_bytes())
        self.assertEqual(data[:8], b"FCEMIDX1")
        count, lookups, records_at, lookup_at, strings_at, strings_len = struct.unpack_from("<6I", data, 8)
        self.assertEqual((count, lookups), (3785, 4737))
        self.assertEqual((records_at, lookup_at, strings_at, len(data)),
                         (32, 32 + count * 44, 32 + count * 44 + lookups * 12,
                          strings_at + strings_len))
        strings = data[strings_at:]

        def string(start, length):
            self.assertLessEqual(start + length, len(strings))
            return strings[start:start + length].decode("utf8")

        entries = []
        for at in range(count):
            fields = struct.unpack_from("<11I", data, records_at + at * 44)
            entries.append([*(string(fields[i], fields[i + 1]) for i in range(0, 8, 2)), *fields[8:]])
            self.assertLessEqual(fields[8], at)
            self.assertLessEqual(fields[9] + fields[10], generator.BIN.stat().st_size)
        lookup = []
        for at in range(lookups):
            start, length, entry = struct.unpack_from("<3I", data, lookup_at + at * 12)
            self.assertLess(entry, count)
            lookup.append([string(start, length), entry])
        self.assertEqual(lookup, sorted(lookup, key=lambda pair: pair[0].encode("utf8")))
        # Captured from all fields of the original Rust table before replacing
        # it, after direct equality against the pinned generator logical data.
        logical = json.dumps([entries, lookup], ensure_ascii=False, separators=(",", ":")).encode()
        self.assertEqual(hashlib.sha256(logical).hexdigest(),
                         "bef876858b89315f401d1015cf34945a998990e9bada03780d8e8b5a8075c1be")

    def test_generation_is_deterministic_and_png_bundle_is_unchanged(self):
        self.assertEqual(generator.render(), generator.render())
        self.assertEqual(hashlib.sha256(generator.BIN.read_bytes()).hexdigest(),
                         "a0047d686f4b7e2469ccd543cdbddf6b23cdda19a3caf3d81c5438399026cd13")


if __name__ == "__main__":
    unittest.main()
