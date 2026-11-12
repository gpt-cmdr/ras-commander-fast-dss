# dss-vortex test data

## `km_to_m_indriya.txt.gz`

Reference answers for the kilometre-to-metre conversion in `src/units.rs` (`km_to_m`), checked by
`units::tests::every_case_matches_indriya`.

- **Size and hash:** 2,328,079 bytes, SHA-256
  `d84aa7c3f7dbda03313d52c62a8ba1fb375cab006fec8a1d5e5ff03e1d7a517f` (gzip, no file name, no
  time stamp).
- **What it records:** the output of indriya, the units library that HEC-Vortex uses (the Units
  of Measurement reference implementation, BSD-3-Clause). The converter is
  `MetricPrefix.KILO(Units.METRE).getConverterTo(Units.METRE)`, called through
  `UnitConverter.convert(double)`, the overload that `NetcdfDataReader.scaleGrid` calls in
  HEC-Vortex 0.13.3 and 1.1.0. It was run in the Java 21 runtimes bundled with those two
  Vortex releases (Eclipse Adoptium 21.0.9 and 21.0.11, as the file's header lines record). Both
  runtimes gave identical answers on every input.
- **Line format:** `<input bits> <Double.toString(input)> <result bits>`. Bits are the 16 hex
  digits of the IEEE-754 double; where indriya throws, the result is `ERR:` and the exception
  class. Lines that start with `#` are comments.
- **Inputs:** a deterministic set of 1,867,099 doubles, built with fixed random seeds:
  - special values, signed zeros and the extremes of each binade class;
  - every power of two and powers of ten from 1e-320 to 1e308, each with its neighbours, in both
    signs;
  - the lowest 20,000 subnormals in both signs;
  - 1,000,000 uniformly random bit patterns;
  - 200,000 float32 values widened to double;
  - decimal-looking kilometre values `n / 10^k` and their neighbours;
  - exact decimal ties at 15 to 17 significant digits.
- **This file is a subset of that run:** every case where an earlier, approximate implementation
  of the conversion differed from indriya, and every 25th other case (82,224 cases). The test
  also reads the full run when the environment variable `KM_TO_M_INDRIYA_FULL` names a file in
  this format. Without it, only this subset is checked.
- **Distribution:** this file is in the source repository but is left out of the Python source
  distribution (`pyproject.toml`, `[tool.maturin] exclude`). Without it the test prints a
  skip message and passes.
