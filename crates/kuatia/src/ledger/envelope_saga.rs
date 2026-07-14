use super::*;

legend! {
    EnvelopeSaga<LedgerCtx, LedgerError> {
        reserve: ReservePostingsStep,
        finalize: FinalizeTransferStep,
    }
}
