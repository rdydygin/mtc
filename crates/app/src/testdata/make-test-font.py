"""Add a `name` table (family MtcTest, Regular) to ttf-parser's demo.ttf."""
import struct, sys

src, dst = sys.argv[1], sys.argv[2]
d = open(src, "rb").read()
n = struct.unpack(">H", d[4:6])[0]
tables = {}
for i in range(n):
    tag = d[12 + 16 * i:16 + 16 * i]
    _, off, ln = struct.unpack(">III", d[16 + 16 * i:28 + 16 * i])
    tables[tag] = bytearray(d[off:off + ln])

def name_table(names):
    records, strings = b"", b""
    for name_id, text in names:
        s = text.encode("utf-16-be")
        records += struct.pack(">HHHHHH", 3, 1, 0x409, name_id, len(s), len(strings))
        strings += s
    return struct.pack(">HHH", 0, len(names), 6 + len(records)) + records + strings

tables[b"name"] = bytearray(name_table([(1, "MtcTest"), (2, "Regular"), (4, "MtcTest Regular"), (6, "MtcTest-Regular")]))
tables[b"head"][8:12] = b"\0\0\0\0"  # checkSumAdjustment, recomputed below

def checksum(data):
    data = bytes(data) + b"\0" * (-len(data) % 4)
    return sum(struct.unpack(">%dI" % (len(data) // 4), data)) & 0xFFFFFFFF

tags = sorted(tables)
count = len(tags)
es = count.bit_length() - 1
header = struct.pack(">IHHHH", 0x00010000, count, 16 << es, es, count * 16 - (16 << es))
offset = 12 + 16 * count
directory, body = b"", b""
for tag in tags:
    data = bytes(tables[tag])
    directory += tag + struct.pack(">III", checksum(data), offset + len(body), len(data))
    body += data + b"\0" * (-len(data) % 4)
font = bytearray(header + directory + body)
head_at = offset + sum(len(bytes(tables[t])) + (-len(tables[t]) % 4) for t in tags[:tags.index(b"head")])
font[head_at + 8:head_at + 12] = struct.pack(">I", (0xB1B0AFBA - checksum(font)) & 0xFFFFFFFF)
open(dst, "wb").write(font)
print(len(font), "bytes")
