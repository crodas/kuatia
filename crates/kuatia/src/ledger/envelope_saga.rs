use super::*;

legend! {
    EnvelopeSaga<LedgerCtx, SagaError> {
        reserve: ReservePostingsStep,
        finalize: FinalizeTransferStep,
    }
}
