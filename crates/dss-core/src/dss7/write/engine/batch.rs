//! P76: many records in one locked write session (DSS7).
//!
//! A batch is the sequence of single-record writes it lists, made in one session: the
//! lock test, the held lock and the reconciliation of an interrupted write
//! happen once, at the start, and the flush of [`SyncMode::End`] once, at the end. Each
//! record is then written exactly as its own write call writes it, through the same
//! transaction body (`rts_v_body`, `its_v_body`, `pd_body`), from a new engine on the
//! header read again (`Session::next_record`). So every record keeps the ordered
//! steps and its in-progress marker, and the process-death guarantee of
//! `docs/format/dss7.md` holds record by record, as for separate calls.
//!
//! What differs from separate calls, and nothing else:
//! - the HEC lock is held from the first record to the last (separate calls release it
//!   between records), so an HEC program waits for the whole batch, as it waits for a
//!   multi-block series;
//! - [`SyncMode::End`] flushes once, after the last record, instead of once per record.
//!   [`SyncMode::Step`] flushes at every flush point of every record and once at the end.
//!
//! Every record is planned (validated without touching the file) before the session
//! starts; a refused plan writes nothing. A record refused or failed after the session
//! started stops the batch: the records before it stay written (and are flushed in the
//! `End` and `Step` modes, as their own calls would have been), and the failure is
//! returned with its index in [`BatchReport::failed`].
//!
//! [`SyncMode::End`]: crate::dss7::write::SyncMode::End
//! [`SyncMode::Step`]: crate::dss7::write::SyncMode::Step
//! [`BatchReport::failed`]: crate::dss7::write::BatchReport::failed

use super::pd::{pd_body, PdPlan};
use super::variants::{its_v_body, rts_v_body};
use super::*;
use crate::dss7::write::variants::{check_extras, TsVariants};
use crate::dss7::write::{BatchItem, BatchReport};

enum Planned {
    Rts(Plan),
    Its(ItsPlan),
    Pd(PdPlan),
}

fn plan_item(item: &BatchItem) -> Result<Planned, WriteError> {
    let none = TsVariants::default();
    Ok(match item {
        BatchItem::Rts(input) => {
            check_extras(input.values.len(), input.quality, &none)?;
            Planned::Rts(plan(input)?)
        }
        BatchItem::Its(input) => {
            check_extras(input.values.len(), input.quality, &none)?;
            Planned::Its(plan_its(input)?)
        }
        BatchItem::Pd(input) => Planned::Pd(pd::plan_pd(input)?),
    })
}

fn body(
    session: &mut Session,
    item: &BatchItem,
    planned: &Planned,
) -> Result<Vec<BlockReport>, WriteError> {
    let none = TsVariants::default();
    match (item, planned) {
        (BatchItem::Rts(input), Planned::Rts(plan)) => {
            let x = check_extras(input.values.len(), input.quality, &none)?;
            rts_v_body(session, input, plan, &x)
        }
        (BatchItem::Its(input), Planned::Its(plan)) => {
            let x = check_extras(input.values.len(), input.quality, &none)?;
            its_v_body(session, input, plan, &x)
        }
        (BatchItem::Pd(_), Planned::Pd(plan)) => pd_body(session, plan),
        _ => Err(WriteError::BadInput(
            "batch plan does not match its item".into(),
        )),
    }
}

/// Write `items` in order through one locked session on `file` (see the module docs).
pub(in crate::dss7::write) fn write_batch(
    file: &File,
    items: &[BatchItem],
    opts: &WriteOptions,
) -> Result<BatchReport, WriteError> {
    let mut planned = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match plan_item(item) {
            Ok(p) => planned.push(p),
            Err(e) => {
                return Ok(BatchReport {
                    reports: Vec::new(),
                    failed: Some((i, e)),
                })
            }
        }
    }
    if items.is_empty() {
        // No record, no write call: the file is not touched (no lock test, no flush).
        return Ok(BatchReport::default());
    }
    let mut session = Session::begin(file, opts)?;
    let mut reports = Vec::with_capacity(items.len());
    for (i, (item, p)) in items.iter().zip(&planned).enumerate() {
        let result = if i == 0 {
            body(&mut session, item, p)
        } else {
            session
                .next_record()
                .and_then(|()| body(&mut session, item, p))
        };
        match result {
            Ok(blocks) => reports.push(session.report(blocks)),
            Err(e) => {
                // The records before this one are complete; flush them as their own calls
                // would have been flushed, then release the lock.
                session.end()?;
                return Ok(BatchReport {
                    reports,
                    failed: Some((i, e)),
                });
            }
        }
    }
    session.end()?;
    Ok(BatchReport {
        reports,
        failed: None,
    })
}
