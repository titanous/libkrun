import importlib.util
from pathlib import Path
import struct
import unittest

spec = importlib.util.spec_from_file_location('receiver_image', Path(__file__).with_name('receiver-image.py'))
receiver_image = importlib.util.module_from_spec(spec)
spec.loader.exec_module(receiver_image)


class ReceiverImageTests(unittest.TestCase):
    def image(self):
        data = bytearray(16384)
        data[:2] = b'MZ'
        struct.pack_into('<I', data, 0x3c, 0x80)
        data[0x80:0x84] = b'PE\0\0'
        struct.pack_into('<HH', data, 0x84, 0xaa64, 1)
        struct.pack_into('<H', data, 0x94, 0x80)
        struct.pack_into('<I', data, 0xa8, 0x1000)
        struct.pack_into('<8sIIII', data, 0x118, b'.text', 0x3000, 0x1000, 0x3000, 0x1000)
        struct.pack_into('<I', data, 0x118 + 36, 0x20000000)
        return data

    def test_injects_aligned_linked_code(self):
        original = self.image()
        result = receiver_image.inject(original, b'code')
        self.assertEqual(result[0x2000:0x2004], b'code')
        self.assertEqual(struct.unpack_from('<I', result, 0x1000)[0], 0x14000400)
        self.assertEqual(len(result), len(original))
        self.assertEqual(original[0x1000:0x1004], b'\0' * 4)

    def test_rejects_unbounded_and_nonexecutable_sections(self):
        for offset, value in [(0x118 + 20, 0x2000), (0x118 + 36, 0), (0x118 + 16, 0x1001), (0xa8, 0x1001)]:
            data = self.image()
            struct.pack_into('<I', data, offset, value)
            with self.assertRaises(ValueError):
                receiver_image.inject(data, b'code')
        with self.assertRaises(ValueError):
            receiver_image.inject(self.image(), bytes(0x3000))

    def test_truncated_headers_refused(self):
        data = self.image()
        for length in range(0x118 + 40):
            with self.assertRaises(ValueError):
                receiver_image.inject(data[:length], b'code')


if __name__ == '__main__':
    unittest.main()
