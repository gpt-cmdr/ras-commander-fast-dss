//! Read-only catalog + grid decoding timing. No output serialization in timed loop.
//! Usage: grid_timing <6|7> <input.dss>
use dss_core::{dss6::Dss6File, dss7::Dss7File, Grid, RecordStatus};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("grid_timing <6|7> <input.dss>".into());
    }
    let before = std::fs::metadata(&args[2])?;
    let start = Instant::now();
    let mut records = 0u64;
    let mut cells = 0u64;
    let mut consume = |g: Grid| {
        records += 1;
        cells += g.data.len() as u64;
        std::hint::black_box(g);
    };
    if args[1] == "6" {
        let f = Dss6File::open(&args[2])?;
        let c = f.catalog()?;
        for e in c.live().filter(|e| (400..450).contains(&e.record_type)) {
            let i = f.read_info(
                dss_core::dss6::source::byte_offset_word(e.info_address)
                    .ok_or("bad info offset")?,
                e.pathname.len() as i32,
            )?;
            consume(f.decode_grid_info(&i)?);
        }
    } else {
        let mut f = Dss7File::open(&args[2])?;
        let c = f.catalog()?;
        for (i, e) in c.catalog.entries.iter().enumerate() {
            if matches!(e.status, RecordStatus::Live | RecordStatus::Alias)
                && (400..450).contains(&e.record_type)
            {
                let r = f.record_at(&c, i)?;
                consume(f.read_grid_record(&r)?);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let after = std::fs::metadata(&args[2])?;
    println!(
        "{{\"records\":{records},\"cells\":{cells},\"seconds\":{elapsed},\"unchanged\":{}}}",
        before.len() == after.len() && before.modified()? == after.modified()?
    );
    Ok(())
}
