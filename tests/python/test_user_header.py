"""DSS6 time-series user header.

The fixture ``synthetic_userheader6.dss`` was written by HEC Monolith 3.3.27
through its Java API; the expected texts and
items below are what Monolith itself returns for the same reads.
"""

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from conftest import fixture_path

FIX = "synthetic_userheader6.dss"

GAGE_A = (
    "State: Example State;StreamSample Creek;DrainageArea: 12.50;Operator: USGS;"
    "GageDatum: 10.25;VerticalDatum: NAVD88;County: Sample;LocationSample Bridge, EX;"
    "DataLocation: Sample Bridge, EX;USGS No: 00000001;HUC: 00000000"
)
GAGE_A_ITEMS = {
    "State": " Example State",
    "DrainageArea": " 12.50",
    "Operator": " USGS",
    "GageDatum": " 10.25",
    "VerticalDatum": " NAVD88",
    "County": " Sample",
    "DataLocation": " Sample Bridge, EX",
    "USGS No": " 00000001",
    "HUC": " 00000000",
}


@pytest.fixture
def f():
    with dss.open(fixture_path(FIX)) as fh:
        yield fh


def test_record_keys_and_monolith_text(f):
    d = f.read_record_arrays("/P26/GAGE A/FLOW/01JAN2020/1HOUR/SYNTH/")
    assert d["version"] == 6
    assert d["user_header_text"] == GAGE_A
    assert d["user_header_items"] == GAGE_A_ITEMS
    # Keywords written without a colon are not items (Monolith drops them too) ...
    assert "Stream" not in d["user_header_items"]
    assert not any(k.startswith("Location") for k in d["user_header_items"])
    # ... but stay in the text.
    assert "StreamSample Creek" in d["user_header_text"]
    w = d["user_header_words"]
    assert w.dtype == np.int32 and len(w) == 55
    raw = d["user_header_bytes"]
    assert raw == w.astype("<i4").tobytes()
    # DSS6 stores the text blank-padded to whole words, without a NUL.
    assert raw.rstrip(b" ") == GAGE_A.encode("ascii") and b"\0" not in raw
    assert d["user_header_block"] == "/P26/GAGE A/FLOW/01JAN2020/1HOUR/SYNTH/"
    assert d["user_header_conflict"] is False


def test_whole_word_header_and_trailing_blanks(f):
    # 32 characters, exactly 8 words: no padding, no NUL.
    d = f.read_record_arrays("/P26/GAGE C/FLOW/01JAN2020/1HOUR/SYNTH/")
    assert len(d["user_header_words"]) == 8
    assert d["user_header_text"] == "Block: january;Operator: Example"
    d = f.read_record_arrays("/P26/GAGE B/FLOW/01JAN2020/1DAY/SYNTH/")
    assert d["user_header_text"] == "VerticalDatum: NGVD29;Note: 4byte"
    assert d["user_header_bytes"].endswith(b"   ")


def test_parser_edge_cases_match_monolith(f):
    d = f.read_record_arrays("/P26/GAGE E/FLOW/01JAN2020/1DAY/SYNTH/")
    assert d["user_header_text"] == "Key: first;Key: second;Time: 12:00;;  Lead: x  ;NoColonHere"
    # Monolith: last repeat wins, "Time: 12:00" (two colons) and "NoColonHere" dropped,
    # leading blanks kept, trailing blanks removed.
    assert d["user_header_items"] == {"Key": " second", "  Lead": " x"}
    assert list(d["user_header_items"]) == ["Key", "  Lead"]
    assert dss.parse_user_header_items(d["user_header_text"]) == d["user_header_items"]


def test_python_parser():
    p = dss.parse_user_header_items
    assert p(None) == {} and p("") == {}
    assert p("A: 1;;B:2 ;C;D:e:f;E:") == {"A": " 1", "B": "2", "E": ""}
    assert p(GAGE_A) == GAGE_A_ITEMS


def test_no_header(f):
    d = f.read_record_arrays("/P26/GAGE F/FLOW/01JAN2020/1DAY/SYNTH/")
    assert len(d["user_header_words"]) == 0
    assert d["user_header_bytes"] == b""
    assert d["user_header_text"] == "" and d["user_header_items"] == {}
    assert d["user_header_encoding"] is None  # P28: no text, no encoding
    df = f.read_timeseries("/P26/GAGE F/FLOW//1DAY/SYNTH/", extra_attrs=True)
    assert "user_header_text" not in df.attrs
    assert "user_header_items" not in df.attrs
    assert "user_header_encoding" not in df.attrs
    assert "user_header_conflict" not in df.attrs


def test_non_ascii_bytes_are_utf8(f):
    # Monolith's JNI writer stored "Café" as doubly UTF-8 encoded bytes. They are valid
    # UTF-8 for "CafÃ©", which is what Monolith reads back.
    # Since P28 the engine decodes UTF-8 too; P26 decoded Latin-1 (4 characters).
    d = f.read_record_arrays("/P26/GAGE G/FLOW/01JAN2020/1DAY/SYNTH/")
    assert b"Caf\xc3\x83\xc2\xa9" in d["user_header_bytes"]
    assert d["user_header_text"] == "Note: CafÃ©"
    assert d["user_header_encoding"] == "utf-8"
    df = f.read_timeseries("/P26/GAGE G/FLOW/01JAN2020/1DAY/SYNTH/", extra_attrs=True)
    assert df.attrs["user_header_text"] == "Note: CafÃ©"
    assert df.attrs["user_header_encoding"] == "utf-8"


def test_dataset_regular_reports_last_block_and_flags_conflict(f):
    d = f.read_arrays("/P26/GAGE C/FLOW//1HOUR/SYNTH/")
    assert d["user_header_text"] == "Block: february;Operator: Example"
    assert d["user_header_block"] == "/P26/GAGE C/FLOW/01FEB2020/1HOUR/SYNTH/"
    assert d["user_header_conflict"] is True
    df = f.read_timeseries("/P26/GAGE C/FLOW//1HOUR/SYNTH/", extra_attrs=True)
    assert df.attrs["user_header_text"] == "Block: february;Operator: Example"
    assert df.attrs["user_header_items"] == {"Block": " february", "Operator": " Example"}
    assert df.attrs["user_header_conflict"] is True
    # A window inside January reads only the January block.
    d = f.read_arrays("/P26/GAGE C/FLOW//1HOUR/SYNTH/", "2020-01-31 20:00", "2020-01-31 23:00")
    assert d["user_header_text"] == "Block: january;Operator: Example"
    assert d["user_header_conflict"] is False


def test_dataset_agreeing_blocks(f):
    for p in ("/P26/GAGE A/FLOW//1HOUR/SYNTH/", "/P26/GAGE A/STAGE//IR-MONTH/SYNTH/"):
        d = f.read_arrays(p)
        assert d["blocks_found"] == 2
        assert d["user_header_text"] == GAGE_A
        assert d["user_header_conflict"] is False
        df = f.read_timeseries(p, extra_attrs=True)
        assert df.attrs["user_header_items"] == GAGE_A_ITEMS
        assert "user_header_conflict" not in df.attrs
        # Default attrs are unchanged.
        assert "user_header_text" not in f.read_timeseries(p).attrs


def test_dataset_irregular_last_window_block(f):
    p = "/P26/GAGE D/STAGE//IR-MONTH/SYNTH/"
    d = f.read_arrays(p)
    assert d["user_header_text"] == "Block: march;VerticalDatum: NAVD88"
    assert d["user_header_conflict"] is True
    # The window ends in February, which has no block: HEC (zreadx6 NUHEAD = 0) and
    # Monolith report no header.
    d = f.read_arrays(p, "2020-01-09", "2020-02-15 23:59")
    assert len(d["times"]) == 2
    assert d["user_header_text"] == "" and d["user_header_block"] is None
    assert len(d["user_header_words"]) == 0
    assert d["user_header_conflict"] is True
    d = f.read_arrays(p, "2020-01-09", "2020-03-06 23:59")
    assert d["user_header_text"] == "Block: march;VerticalDatum: NAVD88"


def test_vertical_datum_stays_informational(f):
    # A datum named in the user header never becomes a typed field.
    d = f.read_record_arrays("/P26/GAGE A/STAGE/01JAN2020/1HOUR/SYNTH/")
    assert d["user_header_items"]["VerticalDatum"] == " NAVD88"
    assert not any("datum" in k.lower() for k in d if not k.startswith("user_header"))


def test_dss7_keys_are_none():
    # DSS7 user headers are out of scope for P26 (dss7.md §12): every key is None.
    with dss.open(fixture_path("sample7.dss")) as fh:
        cat = fh.catalog()
        p = cat.loc[cat["record_type"].isin([100, 105, 110, 115]), "pathname"].iloc[0]
        d = fh.read_record_arrays(p)
        for k in (
            "user_header_words",
            "user_header_bytes",
            "user_header_text",
            "user_header_items",
            "user_header_block",
            "user_header_conflict",
        ):
            assert d[k] is None


def test_existing_dss6_fixtures_have_keys():
    with dss.open(fixture_path("sample6.dss")) as fh:
        cat = fh.catalog()
        p = cat.loc[cat["record_type"].isin([100, 110]), "pathname"].iloc[0]
        d = fh.read_record_arrays(p)
        assert isinstance(d["user_header_text"], str)
        assert isinstance(d["user_header_items"], dict)


# -- NUL handling -------------------------------------------------------------------

NUL_FIX = "synthetic_userheader_nul6.dss"  # crates/dss-core/tests/dss6_user_header.rs


@pytest.mark.parametrize(
    "path", ["/P26/NUL/FLOW/01JAN2000/1HOUR/SYN/", "/P26/NUL/STAGE/01JAN2000/IR-MONTH/SYN/"]
)
def test_nul_terminated_header(path):
    # Text, NUL, then junk ("junk;Hidden: 1") that must not reach the text or items.
    with dss.open(fixture_path(NUL_FIX)) as fh:
        d = fh.read_record_arrays(path)
    raw = d["user_header_bytes"]
    assert raw.startswith(b"Gage: N1;VerticalDatum: NGVD29;\0junk;Hidden: 1")
    assert d["user_header_text"] == "Gage: N1;VerticalDatum: NGVD29;"
    assert d["user_header_items"] == {"Gage": " N1", "VerticalDatum": " NGVD29"}


def test_nul_terminated_header_dataset():
    with dss.open(fixture_path(NUL_FIX)) as fh:
        d = fh.read_arrays("/P26/NUL/FLOW//1HOUR/SYN/")
        assert d["user_header_text"] == "Gage: N1;VerticalDatum: NGVD29;"
        df = fh.read_timeseries("/P26/NUL/FLOW//1HOUR/SYN/", extra_attrs=True)
        assert "Hidden" not in df.attrs["user_header_text"]


def test_regular_window_into_absent_block_keeps_last_found(f):
    # Regular rule (zrrtsi.f): an absent final block leaves the last found header;
    # Monolith also reports February here. Contrast test_dataset_irregular_last_window_block.
    d = f.read_arrays("/P26/GAGE C/FLOW//1HOUR/SYNTH/", "2020-01-31 22:00", "2020-03-05 23:59")
    assert d["blocks_missing"] >= 1
    assert d["user_header_text"] == "Block: february;Operator: Example"
    assert d["user_header_block"] == "/P26/GAGE C/FLOW/01FEB2020/1HOUR/SYNTH/"
    assert d["user_header_conflict"] is True


def test_empty_keyword_or_value_kept():
    # Monolith raises on these; the engine keeps the item with "".
    assert dss.parse_user_header_items(":v;K:;B:   ") == {"": "v", "K": "", "B": ""}
