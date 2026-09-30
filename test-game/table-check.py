#!/usr/bin/env python3
"""Read-only check of ParticleFleet.CT's pointers: finds the table's AOB
patterns in the game's JIT code and resolves gems without injecting code.
Usage: table-check.py <pid>"""
import re
import struct
import sys

pid = int(sys.argv[1])
# 8B 05 <static>   mov eax,[static]      (the instruction before the table's gemsRead)
# 8B 80 6C 06 00 00 85 C0 75 ?? 83 EC 0C 57
gems = re.compile(rb"\x8b\x05(....)\x8b\x80\x6c\x06\x00\x00\x85\xc0\x75.\x83\xec\x0c\x57", re.S)
omni = re.compile(rb"\x8b\x87\x10\x06\x00\x00\x8b\x8f\x40\x05\x00\x00\x8b\xd1\x39\x12", re.S)

with open(f"/proc/{pid}/maps") as f:
    regions = [l.split() for l in f]
mem = open(f"/proc/{pid}/mem", "rb", 0)


def read(addr, n):
    mem.seek(addr)
    return mem.read(n)


for r in regions:
    if "x" not in r[1]:
        continue
    start, end = (int(x, 16) for x in r[0].split("-"))
    try:
        data = read(start, end - start)
    except OSError:
        continue
    for m in gems.finditer(data):
        static = struct.unpack("<I", m.group(1))[0]
        base = struct.unpack("<I", read(static, 4))[0]
        value = struct.unpack("<i", read(base + 0x66C, 4))[0]
        print(f"gems code at 0x{start + m.start():x}: [0x{static:x}] = 0x{base:x}, gems at 0x{base + 0x66C:x} = {value}")
    for m in omni.finditer(data):
        print(f"omni code at 0x{start + m.start():x} (base comes from edi at runtime)")
