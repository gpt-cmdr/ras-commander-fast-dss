"""A GeoTIFF writer for the sign tests in test_gridded.py.

Copied unchanged in behaviour from the private test-oracle tree (`import_cases._sign_tiff`), so the
public tests do not need it.
"""
import struct
from pathlib import Path


def sign_tiff(path: Path, rows: list, origin_x: float, dx: float, origin_y: float, dy: float, marker=True) -> None:
    """A float32 GeoTIFF in EPSG:5070 whose ModelTransformationTag states any sign, written here because no
    library's writer will state negative dx. Only tags GDAL and this engine's reader both need. With
    `marker=False` it states no nodata at all."""
    ny, nx = len(rows), len(rows[0])
    data = b"".join(struct.pack("<f", v) for row in rows for v in row)
    matrix = struct.pack("<16d", dx, 0, 0, origin_x, 0, dy, 0, origin_y, 0, 0, 0, 0, 0, 0, 0, 1)
    keys = struct.pack("<16H", 1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 5070)
    stated = marker
    marker = b"-9999" + bytes(1)
    count = 13 if stated else 12
    at_data = 8 + 2 + count * 12 + 4
    at_matrix = at_data + len(data)
    at_keys = at_matrix + len(matrix)
    at_marker = at_keys + len(keys)
    tags = [(256, 4, 1, nx), (257, 4, 1, ny), (258, 3, 1, 32), (259, 3, 1, 1), (262, 3, 1, 1), (273, 4, 1, at_data),
            (277, 3, 1, 1), (278, 4, 1, ny), (279, 4, 1, len(data)), (339, 3, 1, 3), (34264, 12, 16, at_matrix),
            (34735, 3, 16, at_keys)]
    if stated:
        tags.append((42113, 2, len(marker), at_marker))
    out = b"II*" + bytes([0]) + struct.pack("<I", 8) + struct.pack("<H", count)
    for tag, kind, n, value in tags:
        out += struct.pack("<HHII", tag, kind, n, value)
    out += struct.pack("<I", 0) + data + matrix + keys + (marker if stated else b"")
    path.write_bytes(out)
