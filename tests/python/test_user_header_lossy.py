"""DSS6 user-header text that is invalid UTF-8.

Text that is neither standard nor Java
modified UTF-8 decodes as UTF-8 with each malformed sequence replaced by one U+FFFD,
exactly as Java's decoder does (``new String(bytes, UTF_8)``); the encoding is
``"utf-8-replaced"``. P28 decoded such text as whole-text Latin-1.

``synthetic_userheader_lossy6.dss`` was written by HEC Monolith 3.3.27 through heclib
``ZSRTSX`` and is not
regenerated; its SHA-256 is pinned here. Expected texts are Java 17.0.11
``new String(text, UTF_8)``; Monolith's own read (JNI ``NewStringUTF``) differs and is
pinned as a strict xfail.
"""

import hashlib

import pytest

import ras_commander_fast_dss as dss
from conftest import fixture_path

FIX = "synthetic_userheader_lossy6.dss"
SHA256 = "3e70c21c18642da0fbb5f28ffb2ca4cbc7b695284f554301c9082cd0a4de31c5"
R = "�"

# case -> (text bytes before the NUL, engine text, Monolith supplementalInfo)
CASES = {
    "LONE-CONT": (b"A\x80B", f"A{R}B", "A\x80"),
    "LONE-CONT-END": (b"ABC\xbf", f"ABC{R}", "ABC"),
    "TRUNC2": (b"A\xc3B", f"A{R}B", "A\xc3B"),
    "TRUNC3": (b"A\xe2\x82B", f"A{R}B", "A\xe2\x82"),
    "TRUNC4": (b"A\xf0\x9f\x8cB", f"A{R}B", "A\xf0\x9f"),
    "OVERLONG2": (b"A\xc1\x81B", f"A{R}{R}B", "AAB"),
    "OVERLONG2-C0AF": (b"A\xc0\xafB", f"A{R}{R}B", "A/B"),
    "OVERLONG3": (b"A\xe0\x80\xafB", f"A{R}{R}{R}B", "A/B"),
    "OVERLONG4": (b"A\xf0\x80\x80\xafB", f"A{R}{R}{R}{R}B", "A\xf0\x80"),
    "SURROGATE": (b"A\xed\xa0\x80B", f"A{R}B", "A\ud800B"),
    "SURROGATE-LOW": (b"A\xed\xb0\x80B", f"A{R}B", "A\udc00B"),
    "ABOVE-10FFFF": (b"A\xf4\x90\x80\x80B", f"A{R}{R}{R}{R}B", "A\xf4\x90"),
    "F5-FF": (
        b"A\xf5\xf6\xf7\xf8\xf9\xfa\xfb\xfc\xfd\xfe\xffB",
        "A" + R * 11 + "B",
        "A\xf5\xf6\xf7\xf8\xf9\xfa\xfb\xfc\xfd\xfe\xffB",
    ),
    "F5-SEQ": (b"A\xf5\x80\x80\x80B", f"A{R}{R}{R}{R}B", "A\xf5\x80"),
    "MIXED": (
        b"N: \xc3\xbd\xe9\xe2\x82\xac\x80\xf0\x9f\x8c\x8a;Y: \xc3\xbd",
        f"N: \xfd{R}€{R}\U0001f30a;Y: \xfd",
        "N: \xfd\xe9€\x80\xf0\x9f\x8c\x8a;",
    ),
    "MIXED-LATIN1": (b"Caf\xe9; Y\xc3\xbd", f"Caf{R}; Y\xfd", "Caf\xe9; Y\xfd"),
    "MUTF8-PAIR-INVALID": (
        b"A\xed\xa0\xbc\xed\xbc\x8a \xe9",
        f"A{R}{R} {R}",
        "A\U0001f30a \xe9",
    ),
    "C080-INVALID": (b"A\xc0\x80B\xe9", f"A{R}{R}B{R}", "A\x00B\xe9"),
    "NUL-CUT": (b"Ab\xc3", f"Ab{R}", "Ab\xc3"),
    "NUL-CUT3": (b"Ab\xe2\x82", f"Ab{R}", "Ab\xe2"),
    # The trim runs before decoding, so the blanks are not text.
    "TRIM": (b"Ab\xe2\x82   ", f"Ab{R}", "Ab\xe2\x82  "),
    "TRIM-CONT": (b"Ab\xe2\x82\xac\x80  ", f"Ab€{R}", "Ab€\x80 "),
    "VALID4": (b"A\xf0\x9f\x8c\x8aB", "A\U0001f30aB", "A\xf0\x9f"),
    "CONT-RUN": (b"A\x80\x80\x80\x80BCDE", f"A{R}{R}{R}{R}BCDE", "A\x80\x80\x80\x80"),
}


def _path(case):
    return f"/P31/{case}/FLOW/01JAN2020/1DAY/SYNTH/"


@pytest.fixture(scope="module")
def f():
    with dss.open(fixture_path(FIX)) as fh:
        yield fh


def test_fixture_sha256():
    assert hashlib.sha256(fixture_path(FIX).read_bytes()).hexdigest() == SHA256


@pytest.mark.parametrize("case", list(CASES))
def test_text_follows_java(f, case):
    stored, text, _ = CASES[case]
    enc = "utf-8" if case == "VALID4" else "utf-8-replaced"
    d = f.read_record_arrays(_path(case))
    assert d["user_header_text"] == text
    assert d["user_header_encoding"] == enc
    # Raw bytes unchanged: the text, then the NUL (and, for NUL-CUT, the bytes after it).
    assert d["user_header_bytes"].startswith(stored + b"\x00")
    assert d["user_header_items"] == dss.parse_user_header_items(text)
    # heclib ZSRTSX stores no coordinates, so these records have no DSS6 location; the
    # location supplemental decode is checked on synthetic_userheader_utf8_6.dss.
    assert f.read_associated_location(_path(case)) is None


def test_attrs_and_items_from_replaced_text(f):
    df = f.read_timeseries(_path("MIXED"), extra_attrs=True)
    assert df.attrs["user_header_text"] == CASES["MIXED"][1]
    assert df.attrs["user_header_encoding"] == "utf-8-replaced"
    assert f.read_record_arrays(_path("MIXED"))["user_header_items"] == {
        "N": f" \xfd{R}€{R}\U0001f30a",
        "Y": " \xfd",
    }


@pytest.mark.xfail(
    strict=True,
    reason=(
        "Monolith reads the text with JNI NewStringUTF; HotSpot maps each "
        "bad byte to its Latin-1 character (no U+FFFD) and truncates the string to its "
        "count of non-continuation bytes. The engine follows Java's UTF-8 decoder."
    ),
)
@pytest.mark.parametrize("case", list(CASES))
def test_text_equals_monolith(f, case):
    _, _, mono = CASES[case]
    assert f.read_record_arrays(_path(case))["user_header_text"] == mono.rstrip(" ")
