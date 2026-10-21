#!/usr/bin/env python3
"""Print the metadata of an .rpm file without the rpm tools.

Reads the lead, the signature header and the main header (RPM v4 format) and
prints one JSON object: name, version, release, arch, files (path, mode),
requires, conflicts, post-install script. Used by scripts/p6_gate.sh.
"""
import json
import struct
import sys

TAGS = {
    1000: "name", 1001: "version", 1002: "release", 1022: "arch",
    1024: "postin", 1030: "filemodes", 1039: "fileusernames",
    1049: "requirename", 1050: "requireversion", 1054: "conflictname",
    1116: "dirindexes", 1117: "basenames", 1118: "dirnames", 1125: "payloadcompressor",
}


def read_header(data, pos):
    magic, _ver, _res, nindex, hsize = struct.unpack(">3sB4sII", data[pos:pos + 16])
    if magic != b"\x8e\xad\xe8":
        raise SystemExit("not an rpm header at offset %d" % pos)
    idx = pos + 16
    store = idx + nindex * 16
    out = {}
    for i in range(nindex):
        tag, typ, off, count = struct.unpack(">IIII", data[idx + i * 16: idx + i * 16 + 16])
        if tag not in TAGS:
            continue
        p = store + off
        if typ in (6, 9):  # STRING, I18NSTRING
            out[TAGS[tag]] = data[p:data.index(b"\0", p)].decode()
        elif typ == 8:  # STRING_ARRAY
            vals = []
            for _ in range(count):
                e = data.index(b"\0", p)
                vals.append(data[p:e].decode())
                p = e + 1
            out[TAGS[tag]] = vals
        elif typ == 4:  # INT32
            out[TAGS[tag]] = list(struct.unpack(">%dI" % count, data[p:p + 4 * count]))
        elif typ == 3:  # INT16
            out[TAGS[tag]] = list(struct.unpack(">%dH" % count, data[p:p + 2 * count]))
    return out, store + hsize


def main(path):
    data = open(path, "rb").read()
    if data[:4] != b"\xed\xab\xee\xdb":
        raise SystemExit("not an rpm file")
    _sig, pos = read_header(data, 96)
    pos += (8 - (pos - 96) % 8) % 8  # the signature header is padded to 8 bytes
    hdr, _end = read_header(data, pos)
    files = []
    for base, di, mode in zip(hdr.get("basenames", []), hdr.get("dirindexes", []), hdr.get("filemodes", [])):
        files.append({"path": hdr["dirnames"][di] + base, "mode": oct(mode)})
    requires = list(zip(hdr.get("requirename", []), hdr.get("requireversion", [])))
    print(json.dumps({
        "name": hdr.get("name"), "version": hdr.get("version"), "release": hdr.get("release"),
        "arch": hdr.get("arch"), "files": files, "requires": requires,
        "conflicts": hdr.get("conflictname", []), "postin": hdr.get("postin", ""),
        "payload": hdr.get("payloadcompressor"),
    }, indent=1))


if __name__ == "__main__":
    main(sys.argv[1])
