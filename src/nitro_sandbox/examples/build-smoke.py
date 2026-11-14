#!/usr/bin/env python3
"""Build a test EIF from an existing EIF kernel and a static guest verifier."""
import pathlib
import struct
import sys
import zlib
source = pathlib.Path(sys.argv[1]).read_bytes()
if source[:4] == b'.eif':
    h = struct.unpack('>4sHHQQHH32Q32QII', source[:548])
    at = h[7]
    size = struct.unpack('>HHQ', source[at:at+12])[2]
    kernel = source[at+12:at+12+size]
else:
    kernel = source
program = pathlib.Path(sys.argv[2]).read_bytes()
def cpio(name, data, mode, ino):
    name = name.encode()+b'\0'
    fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name), 0]
    result = b'070701'+b''.join(f'{v:08x}'.encode() for v in fields)+name
    result += b'\0'*(-len(result)%4)
    result += data
    result += b'\0'*(-len(data)%4)
    return result
initrd = cpio('init', program, 0o100755, 1)+cpio('TRAILER!!!', b'', 0, 2)
sections = [(1,kernel),(2,b'console=ttyS0 panic=-1 reboot=k rdinit=/init'),(3,initrd),(5,b'{}')]
body = bytearray(); offsets=[0]*32; sizes=[0]*32
for i,(kind,data) in enumerate(sections):
    offsets[i]=548+len(body); sizes[i]=len(data)
    body += struct.pack('>HHQ',kind,0,len(data))+data
header=struct.pack('>4sHHQQHH32Q32QII',b'.eif',4,0xa41,1<<30,2,0,len(sections),*offsets,*sizes,0,0)
image=bytearray(header)+body
image[544:548]=struct.pack('>I',zlib.crc32(image[548:],zlib.crc32(image[:544])))
pathlib.Path(sys.argv[3]).write_bytes(image)
