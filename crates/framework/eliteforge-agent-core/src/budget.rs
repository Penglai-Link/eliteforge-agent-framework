use crate::{ModelError, Usage};
use std::sync::{Arc, Mutex};

/// Shared logical ReAct admission across parallel nodes of one DAG run.
#[doc(hidden)]
pub struct ExecutionCounters {
    pub steps: std::sync::atomic::AtomicUsize,
    pub tools: std::sync::atomic::AtomicUsize,
}
impl ExecutionCounters {
    pub fn admit(counter: &std::sync::atomic::AtomicUsize, amount: usize, limit: usize) -> bool {
        counter
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |used| used.checked_add(amount).filter(|next| *next <= limit),
            )
            .is_ok()
    }
}

#[derive(Default)]
#[doc(hidden)]
pub struct UsageLedger {
    pub usage: Usage,
    reserved: u64,
}

impl UsageLedger {
    pub fn including_reservations(&self) -> Usage {
        Usage {
            input_tokens: self.usage.input_tokens.saturating_add(self.reserved),
            output_tokens: self.usage.output_tokens,
        }
    }
}

#[async_trait::async_trait]
#[doc(hidden)]
pub trait BudgetJournal: Send + Sync {
    async fn save(
        &self,
        ledger: Arc<Mutex<UsageLedger>>,
        child_runs: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<(), crate::StateError>;
}

/// Reservations serialize admission across concurrent calls sharing a run.
/// On cancellation/transport failure, keep the reserved estimate charged: a
/// disconnected provider may still complete and bill the request.
#[doc(hidden)]
pub struct UsagePermit {
    ledger: Arc<Mutex<UsageLedger>>,
    reservation: Usage,
    settled: bool,
}

impl UsagePermit {
    /// Reserve a child's entire allowance before dispatch. Children own their
    /// own ledgers; concurrent children cannot spend the same parent capacity.
    pub fn reserve_child(
        ledger: Arc<Mutex<UsageLedger>>,
        requested: u64,
        limit: u64,
    ) -> Result<(Self, u64), ModelError> {
        let mut state = ledger.lock().unwrap_or_else(|e| e.into_inner());
        let charged = state.including_reservations();
        let available =
            limit.saturating_sub(charged.input_tokens.saturating_add(charged.output_tokens));
        let allowance = available.min(requested);
        if allowance == 0 {
            return Err(ModelError::Fatal(
                "parent budget cannot admit a child run".into(),
            ));
        }
        state.reserved = state.reserved.saturating_add(allowance);
        drop(state);
        Ok((
            Self {
                ledger,
                reservation: Usage {
                    input_tokens: 0,
                    output_tokens: allowance,
                },
                settled: false,
            },
            allowance,
        ))
    }
    pub fn reserve(
        ledger: Arc<Mutex<UsageLedger>>,
        input_tokens: u64,
        output_tokens: u32,
        limit: u64,
    ) -> Result<(Self, u32), ModelError> {
        let mut state = ledger.lock().unwrap_or_else(|e| e.into_inner());
        let consumed = state
            .usage
            .input_tokens
            .saturating_add(state.usage.output_tokens)
            .saturating_add(state.reserved);
        let available = limit.saturating_sub(consumed);
        if input_tokens >= available {
            return Err(ModelError::Fatal(
                "total model token budget cannot admit this request".into(),
            ));
        }
        let output_tokens = (output_tokens as u64).min(available - input_tokens) as u32;
        let reservation = Usage {
            input_tokens,
            output_tokens: output_tokens as u64,
        };
        state.reserved = state
            .reserved
            .saturating_add(input_tokens)
            .saturating_add(output_tokens as u64);
        drop(state);
        Ok((
            Self {
                ledger,
                reservation,
                settled: false,
            },
            output_tokens,
        ))
    }

    pub fn settle(&mut self, actual: &Usage, limit: u64) -> Result<(), ModelError> {
        self.settle_usage(actual, limit, true)
    }
    pub fn settle_child(&mut self, actual: &Usage, limit: u64) -> Result<(), ModelError> {
        self.settle_usage(actual, limit, false)
    }
    /// Roll back admission only when dispatch is known not to have occurred.
    pub fn release_unspent(&mut self) {
        if !self.settled {
            let mut state = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
            state.reserved = state.reserved.saturating_sub(
                self.reservation
                    .input_tokens
                    .saturating_add(self.reservation.output_tokens),
            );
            self.settled = true;
        }
    }
    fn settle_usage(
        &mut self,
        actual: &Usage,
        limit: u64,
        unknown_if_zero: bool,
    ) -> Result<(), ModelError> {
        let mut state = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        state.reserved = state.reserved.saturating_sub(
            self.reservation
                .input_tokens
                .saturating_add(self.reservation.output_tokens),
        );
        // Providers without usage cannot silently receive unlimited free calls.
        if unknown_if_zero && actual.input_tokens == 0 && actual.output_tokens == 0 {
            state.usage.add(&self.reservation);
        } else {
            state.usage.add(actual);
        }
        self.settled = true;
        if state
            .usage
            .input_tokens
            .saturating_add(state.usage.output_tokens)
            .saturating_add(state.reserved)
            > limit
        {
            return Err(ModelError::Fatal(
                "provider usage exceeded total model token budget".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for UsagePermit {
    fn drop(&mut self) {
        if !self.settled {
            let mut state = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
            state.reserved = state.reserved.saturating_sub(
                self.reservation
                    .input_tokens
                    .saturating_add(self.reservation.output_tokens),
            );
            state.usage.add(&self.reservation);
        }
    }
}
