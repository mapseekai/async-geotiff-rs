"""Regenerate the tiny synthetic TIFF fixtures using only Python's stdlib."""

from pathlib import Path
import struct
import zlib


def tiff(*, bits=8, bands=3, epsg=4326, deflate=False, sizes=(64, 32), tile_size=32, mask_tile_size=None):
    data = bytearray(b"II\x2a\x00\x08\x00\x00\x00")
    previous_next = None
    levels = [(level, size, False) for level, size in enumerate(sizes)]
    if mask_tile_size is not None:
        levels += [(level, size, True) for level, size in enumerate(sizes)]
    for level, size, is_mask in levels:
        if len(data) % 2:
            data.append(0)
        block = mask_tile_size if is_mask else tile_size
        sample_bits, sample_bands = (1, 1) if is_mask else (bits, bands)
        tiles = (size // block) ** 2
        scale = (0.001 if epsg == 4326 else 100.0) * 2**level
        origin = (10.0, 20.064) if epsg == 4326 else (1113194.9, 2279430.9)
        keys = [1, 1, 0, 3, 1024, 0, 1, 2 if epsg == 4326 else 1,
                1025, 0, 1, 1, 2048 if epsg == 4326 else 3072, 0, 1, epsg]
        # (tag, TIFF type, values); offsets are filled once metadata is written.
        tags = [
            (254, 4, [int(level > 0) | (4 if is_mask else 0)]), (256, 4, [size]), (257, 4, [size]),
            (258, 3, [sample_bits] * sample_bands), (259, 3, [8 if deflate else 1]),
            (262, 3, [4 if is_mask else 2 if bands == 3 else 1]), (277, 3, [sample_bands]),
            (284, 3, [1]), (322, 4, [block]), (323, 4, [block]),
            (324, 4, [0] * tiles), (325, 4, [block**2 * sample_bands * sample_bits // 8] * tiles),
            (339, 3, [1] * sample_bands), (33550, 12, [scale, scale, 0.0]),
            (33922, 12, [0.0, 0.0, 0.0, *origin, 0.0]),
            (34735, 3, keys), (42113, 2, b"0\x00"),
        ]
        ifd = len(data)
        if previous_next is not None:
            struct.pack_into("<I", data, previous_next, ifd)
        tags.sort()
        data.extend(struct.pack("<H", len(tags)))
        data.extend(bytes(12 * len(tags) + 4))
        previous_next = ifd + 2 + 12 * len(tags)
        offset_location = None
        count_location = None
        for index, (tag, kind, values) in enumerate(tags):
            payload = values if kind == 2 else struct.pack("<" + {3: "H", 4: "I", 12: "d"}[kind] * len(values), *values)
            entry = ifd + 2 + 12 * index
            struct.pack_into("<HHI", data, entry, tag, kind, len(values))
            if len(payload) <= 4:
                location = entry + 8
                data[location:location + len(payload)] = payload
            else:
                location = len(data)
                struct.pack_into("<I", data, entry + 8, location)
                data.extend(payload)
                if len(data) % 2:
                    data.append(0)
            if tag == 324:
                offset_location = location
            if tag == 325:
                count_location = location
        for ty in range(size // block):
            for tx in range(size // block):
                tile = ty * (size // block) + tx
                struct.pack_into("<I", data, offset_location + tile * 4, len(data))
                pixels = bytearray()
                for y in range(ty * block, (ty + 1) * block):
                    for x in range(tx * block, (tx + 1) * block):
                        if is_mask:
                            if x % 8 == 0:
                                # Left half valid, right half masked out.
                                pixels.append(sum(int(x + bit < size // 2) << (7 - bit) for bit in range(8)))
                            continue
                        for band in range(bands):
                            value = 0 if x < size // 4 and y >= 3 * size // 4 else 1 + (x * 2**level + 3 * y * 2**level + band * 61) % 254
                            pixels.extend(struct.pack("<B" if bits == 8 else "<H", value if bits == 8 else value * 100))
                payload = zlib.compress(pixels, level=9) if deflate else pixels
                struct.pack_into("<I", data, count_location + tile * 4, len(payload))
                data.extend(payload)
    return bytes(data)


if __name__ == "__main__":
    root = Path(__file__).parent
    for name, options in (("rgb_nodata.tif", {}), ("uint16.tif", {"bits": 16, "bands": 1}), ("projected.tif", {"epsg": 3857}), ("rgb_deflate.tif", {"deflate": True}), ("rgb_uint16_statistics.tif", {"bits": 16, "deflate": True, "sizes": (1792,), "tile_size": 256})):
        (root / name).write_bytes(tiff(**options))
    for data_tile, mask_tile in [(16, 16), (32, 16), (16, 32)]:
        (root / f"mask_{data_tile}_{mask_tile}.tif").write_bytes(tiff(
            bands=1, sizes=(32,), tile_size=data_tile, mask_tile_size=mask_tile,
        ))
