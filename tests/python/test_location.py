"""Location records (type 20) and associated-location metadata.

Expected values come from HEC oracles reading the same public fixtures:
``synthetic_location7.dss`` was written by hecdss 0.1.33 and read back with hecdss
(``hec_dss_locationRetrieve``) and Monolith 3.3.27 (``HecTimeSeries.read``);
``sample6.dss`` (HEC DSSVue sample, DSS6) matches Monolith on all 149 time-series
records with stored coordinates.
"""

import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import (
    DssPathnameNotFoundError,
    DssUnsupportedRecordError,
    Location,
    parse_supplemental,
)

from conftest import fixture_path

LOC7 = "synthetic_location7.dss"
GAGE_A_TS = "/P15 SYNTH/GAGE A/STAGE/01Jan2020/1Hour/P15/"
GAGE_A_LOC = "/P15 SYNTH/GAGE A/Location Info////"


@pytest.fixture
def f7():
    with dss.open(fixture_path(LOC7)) as f:
        yield f


def test_catalog_lists_location_records(f7):
    cat = f7.catalog()
    locs = cat[cat.record_type == 20]
    assert len(locs) == 4
    assert set(locs.record_type_name) == {"LOC"}
    assert all(p.endswith("/Location Info////") for p in locs.pathname)


def test_unknown_typed_datum_is_not_reconciled(f7):
    loc = f7.read_location(GAGE_A_LOC)
    assert isinstance(loc, Location)
    assert loc.source == "dss7_location_record"
    assert loc.record_type == 20 and loc.dss_version == 7
    assert loc.coordinates == (-97.25, 30.5, 612.5)
    assert (loc.coordinate_system, loc.coordinate_system_code) == ("LAT_LONG", 1)
    assert (loc.horizontal_units, loc.horizontal_units_code) == ("DECIMAL_DEGREES", 3)
    assert (loc.horizontal_datum, loc.horizontal_datum_code) == ("NAD83", 1)
    assert (loc.vertical_units, loc.vertical_units_code) == ("FEET", 1)
    # Typed vertical datum 0: unknown, not NAVD88; the text's NGVD29 stays separate.
    assert loc.vertical_datum_code == 0
    assert loc.vertical_datum is None
    assert loc.vertical_datum_unknown is True
    assert loc.supplemental_vertical_datum == "NGVD29"
    assert loc.supplemental_items == {
        "VerticalDatum": "NGVD29",
        "Operator": "Synthetic",
        "Note": "unknown typed datum",
    }
    assert loc.supplemental == "VerticalDatum:NGVD29;Operator:Synthetic;Note:unknown typed datum;"
    assert loc.supplemental_bytes == loc.supplemental.encode("latin-1")
    assert loc.time_zone == "America/Chicago"
    assert loc.descriptors_stored is True
    assert loc.requested_pathname == GAGE_A_LOC
    assert loc.to_dict()["vertical_datum_code"] == 0


def test_named_datums_units_and_time_zones(f7):
    b = f7.read_location("/P15 SYNTH/GAGE B/Location Info////")
    assert (b.coordinate_system, b.coordinate_id) == ("UTM", 15)
    assert (b.horizontal_units, b.horizontal_datum) == ("METERS", "WGS84")
    assert (b.vertical_units, b.vertical_datum, b.vertical_datum_unknown) == ("METERS", "NAVD-88", False)
    assert b.time_zone == "UTC"
    assert b.supplemental == "" and b.supplemental_items == {}  # stored empty

    c = f7.read_location("/p15 synth/gage c/location info////")  # case-insensitive lookup
    assert (c.coordinate_system, c.coordinate_id) == ("STATE_PLANE_FIPS", 4203)
    assert (c.horizontal_units, c.horizontal_datum) == ("FEET", "NAD27")
    assert (c.vertical_datum, c.vertical_datum_unknown) == ("NGVD-29", False)
    assert c.time_zone is None  # empty name: nothing stored
    assert c.supplemental == "line one\nline two"
    assert c.supplemental_items == {}

    d = f7.read_location("/P15 SYNTH/GAGE D/Location Info////")
    assert (d.coordinate_system, d.horizontal_datum) == ("LOCAL", "LOCAL")
    assert (d.horizontal_units, d.horizontal_units_code) == (None, 0)
    assert (d.vertical_units, d.vertical_units_code) == (None, 0)
    # A code outside HEC's table keeps its value and has no name.
    assert (d.vertical_datum, d.vertical_datum_code, d.vertical_datum_unknown) == (None, 9, True)
    assert d.supplemental_vertical_datum == "OTHER"
    assert d.supplemental_items == {"verticalDatum": "OTHER", "Key With Space": "value: with colon"}
    assert d.time_zone == "PST"


def test_associated_location(f7):
    a = f7.read_associated_location(GAGE_A_TS)
    assert a.pathname == GAGE_A_LOC
    assert a.requested_pathname == GAGE_A_TS
    assert a.to_dict() | {"requested_pathname": ""} == f7.read_location(GAGE_A_LOC).to_dict() | {
        "requested_pathname": ""
    }
    # Any D part and the paired-data record share the A/B location.
    assert f7.read_associated_location("/P15 SYNTH/GAGE A/STAGE//1Hour/P15/").coordinates == a.coordinates
    assert f7.read_associated_location("/P15 SYNTH/GAGE A/STAGE-FLOW///P15/").coordinates == a.coordinates
    # A location pathname is its own associated location.
    assert f7.read_associated_location(GAGE_A_LOC).coordinates == a.coordinates
    assert f7.read_associated_location("/P15 SYNTH/NO LOCATION/FLOW/01Jan2020/1Hour/P15/") is None


def test_module_wrappers():
    p = fixture_path(LOC7)
    assert dss.read_location(p, GAGE_A_LOC).vertical_datum_unknown
    assert dss.read_associated_location(p, GAGE_A_TS).time_zone == "America/Chicago"
    assert dss.read_associated_location(p, "/P15 SYNTH/NONE/FLOW////") is None


def test_typed_errors(f7):
    with pytest.raises(DssUnsupportedRecordError):
        f7.read_location(GAGE_A_TS)
    with pytest.raises(DssPathnameNotFoundError):
        f7.read_location("/P15 SYNTH/NOWHERE/Location Info////")
    with pytest.raises(DssPathnameNotFoundError):
        f7.read_associated_location("no slashes")
    # A location record is not a time series or paired data.
    with pytest.raises(DssUnsupportedRecordError):
        f7.read_timeseries(GAGE_A_LOC)
    with pytest.raises(DssUnsupportedRecordError):
        f7.read_paired_data(GAGE_A_LOC)


def test_dss6_header_coordinates():
    with dss.open(fixture_path("sample6.dss")) as f:
        with pytest.raises(DssUnsupportedRecordError):
            f.read_location("/MISSISSIPPI/ST. LOUIS/Location Info////")
        loc = f.read_associated_location("/MISSISSIPPI/ST. LOUIS/FLOW/01JAN1861/1DAY/OBS/")
        assert loc.source == "dss6_ts_header"
        assert loc.dss_version == 6 and loc.record_type == 100
        assert loc.coordinates == (-901047.2, 383744.4, 379.94)
        assert (loc.coordinate_system, loc.horizontal_units) == ("LAT_LONG", "DEGREES_MINUTES_SECONDS")
        assert (loc.horizontal_datum, loc.vertical_units) == ("NAD83", "FEET")
        assert loc.vertical_datum_unknown and loc.vertical_datum is None
        assert loc.time_zone is None and loc.supplemental is None and loc.supplemental_items == {}
        assert loc.program is None and loc.last_write is None
        # No D part: the first matching block in catalog order is used.
        nod = f.read_associated_location("/MISSISSIPPI/ST. LOUIS/FLOW//1DAY/OBS/")
        assert nod is not None and nod.coordinates == loc.coordinates
        # Records without stored coordinates give None; a missing record raises.
        cat = f.catalog()
        ts = cat[(cat.record_type >= 100) & (cat.record_type < 200)]
        found = sum(f.read_associated_location(p) is not None for p in ts.pathname)
        assert found == 149
        with pytest.raises(DssPathnameNotFoundError):
            f.read_associated_location("/NO/SUCH/FLOW/01JAN2000/1DAY/OBS/")


def test_dss6_non_ts_record_has_no_location():
    with dss.open(fixture_path("synthetic_pd6.dss")) as f:
        cat = f.catalog()
        pd_path = cat[cat.record_type.isin([200, 205])].pathname.iloc[0]
        assert f.read_associated_location(pd_path) is None


def test_parse_supplemental():
    assert parse_supplemental("Offset:1.2;Shift:3.4;Transform:LogLog;") == [
        ("Offset", "1.2"),
        ("Shift", "3.4"),
        ("Transform", "LogLog"),
    ]
    assert parse_supplemental(None) == []
    assert parse_supplemental("a:1;note;a:2; b : c ;") == [("a", "1"), ("a", "2"), (" b ", " c ")]


# -- edge cases -----------------------------------------------------------------------

PY6 = "synthetic_python6.dss"
PY7 = "synthetic_python7.dss"
LATIN_B = "/P15 LOC/GAGE \u00c9"


def test_dss6_blocks_must_agree():
    from ras_commander_fast_dss import DssLocationConflictError

    with dss.open(fixture_path(PY6)) as f:
        agree = f.read_associated_location("/P15 LOC/AGREE/FLOW//1HOUR/OBS/")
        assert agree.coordinates == (-97.25, 30.5, 612.5)
        assert agree.requested_pathname == "/P15 LOC/AGREE/FLOW//1HOUR/OBS/"
        with pytest.raises(DssLocationConflictError):
            f.read_associated_location("/P15 LOC/DISAGREE/FLOW//1HOUR/OBS/")
        # Each block stays readable by its full pathname.
        jan = f.read_associated_location("/P15 LOC/DISAGREE/FLOW/01JAN2000/1HOUR/OBS/")
        feb = f.read_associated_location("/P15 LOC/DISAGREE/FLOW/01FEB2000/1HOUR/OBS/")
        assert jan.x == -97.25 and feb.x == -97.5
        # Blocks without coordinates (header words 13-24 zero) have no location.
        assert f.read_associated_location("/TS-BASIN/B/FLOW/01JAN2000/1HOUR/F/") is None


def test_dss6_fallback_reads_catalog_once(monkeypatch):
    calls = []
    real = dss.DssFile.catalog

    def counting(self, *a, **k):
        calls.append(1)
        return real(self, *a, **k)

    monkeypatch.setattr(dss.DssFile, "catalog", counting)
    with dss.open(fixture_path(PY6)) as f:
        for _ in range(50):
            assert f.read_associated_location("/P15 LOC/AGREE/FLOW//1HOUR/OBS/") is not None
            with pytest.raises(DssPathnameNotFoundError):
                f.read_associated_location("/P15 LOC/NONE/FLOW//1HOUR/OBS/")
    assert len(calls) == 1


def test_latin1_a_b_parts():
    with dss.open(fixture_path(PY6)) as f:
        loc = f.read_associated_location(LATIN_B + "/FLOW/01JAN2000/1HOUR/OBS/")
        assert loc.coordinates == (10.0, 20.0, 30.0)
        assert (loc.coordinate_system, loc.vertical_datum) == ("UTM", "NAVD-88")
        assert loc.pathname == LATIN_B + "/FLOW/01JAN2000/1HOUR/OBS/"
        assert f.read_associated_location(LATIN_B + "/FLOW//1HOUR/OBS/").coordinates == loc.coordinates
    with dss.open(fixture_path(PY7)) as f:
        loc = f.read_associated_location(LATIN_B + "/FLOW/01JAN2000/1HOUR/OBS/")
        assert loc is not None and loc.pathname == LATIN_B + "/Location Info////"
        assert loc.coordinates == (5.5, 6.5, 7.5)
        assert (loc.vertical_datum, loc.vertical_datum_named) == ("NGVD-29", True)
        assert loc.time_zone == "UTC" and loc.supplemental_vertical_datum == "NGVD29"
        assert f.read_location(LATIN_B + "/Location Info////").to_dict() | {"requested_pathname": ""} == (
            loc.to_dict() | {"requested_pathname": ""}
        )
        assert f.read_associated_location("/P15 LOC/GAGE E/FLOW////") is None


def test_recover_mode():
    p = fixture_path(LOC7)
    with dss.open(p) as a, dss.open(p, recover=True) as b:
        assert b.recover
        for path in (GAGE_A_TS, "/P15 SYNTH/GAGE B/STAGE//1Hour/P15/", GAGE_A_LOC):
            x, y = a.read_associated_location(path), b.read_associated_location(path)
            assert x.to_dict() == y.to_dict()
        assert b.read_location(GAGE_A_LOC).to_dict() == a.read_location(GAGE_A_LOC).to_dict()
        assert b.read_associated_location("/P15 SYNTH/NO LOCATION/FLOW////") is None
    assert dss.read_associated_location(p, GAGE_A_TS, recover=True).x == -97.25
    with dss.open(fixture_path(PY7), recover=True) as f:
        assert f.read_associated_location(LATIN_B + "/FLOW////").x == 5.5


def test_bad_location_pathname_raises(f7):
    long_b = "/P15 SYNTH/" + "B" * 400 + "/STAGE//1Hour/P15/"
    with pytest.raises(DssPathnameNotFoundError):
        f7.read_associated_location(long_b)
    with pytest.raises(DssPathnameNotFoundError):
        f7.read_associated_location("/only/")


def test_named_versus_other_vertical_datum(f7):
    named = {
        "A": (None, True, False),
        "B": ("NAVD-88", False, True),
        "C": ("NGVD-29", False, True),
        "D": (None, True, False),
    }
    for g, want in named.items():
        loc = f7.read_location(f"/P15 SYNTH/GAGE {g}/Location Info////")
        assert (loc.vertical_datum, loc.vertical_datum_unknown, loc.vertical_datum_named) == want
    # Code 3 (OTHER): typed as other/local, neither unknown nor a usable named datum.
    d = _native_dict(vertical_datum=3)
    other = Location._from_native(d, requested="", dss_file="")
    assert (other.vertical_datum, other.vertical_datum_unknown, other.vertical_datum_named) == (
        "OTHER",
        False,
        False,
    )


def _native_dict(**over):
    d = dict(
        version=7, pathname="/A/B/Location Info////", record_type=20,
        source="dss7_location_record", x=0.0, y=0.0, z=0.0, descriptors_stored=True,
        coordinate_system=0, coordinate_id=0, horizontal_units=0, horizontal_datum=0,
        vertical_units=0, vertical_datum=0, time_zone=None, supplemental=None,
        values1=None, program=None, last_write_ms=None,
    )
    d.update(over)
    # P28: the extension supplies the text decoded in dss-core (DSS7: Latin-1).
    sup = d["supplemental"]
    d.setdefault("supplemental_text", None if sup is None else bytes(sup).decode("latin-1"))
    d.setdefault("supplemental_encoding", None if sup is None else "latin-1")
    return d


def test_supplemental_vertical_datum_anywhere():
    from ras_commander_fast_dss import find_supplemental_keyword

    text = b"USGS gage 12\nVERTICALDATUM:NGVD29\nverticalDatumInfo:xyz"
    loc = Location._from_native(_native_dict(supplemental=text), requested="", dss_file="")
    assert loc.supplemental_vertical_datum == "NGVD29"
    assert loc.vertical_datum_unknown and not loc.vertical_datum_named  # typed field unchanged
    assert find_supplemental_keyword("a:1; x verticalDatum:OTHER;b:2", "verticalDatum") == "OTHER"
    assert find_supplemental_keyword("verticalDatumInfo:abc;", "verticalDatum") is None
    assert find_supplemental_keyword(None, "verticalDatum") is None


def test_values1_exposed(f7):
    loc = f7.read_location(GAGE_A_LOC)
    assert isinstance(loc.values1, tuple) and len(loc.values1) == 16
    assert loc.values1[6:12] == (1, 0, 3, 1, 1, 0)
    with dss.open(fixture_path("sample6.dss")) as f:
        assert f.read_associated_location("/MISSISSIPPI/ST. LOUIS/FLOW/01JAN1861/1DAY/OBS/").values1 is None


def test_regenerated_python6_has_no_blank_coordinates():
    with dss.open(fixture_path(PY6)) as f:
        for p in ("/TS-BASIN/B/FLOW/01JAN2000/1HOUR/F/", "/A/B/FLOW/01JAN2000/1HOUR/TS-OBS/",
                  "/A/B/FLOW/TS-PATTERN/1HOUR/PAT/"):
            assert f.read_associated_location(p) is None
