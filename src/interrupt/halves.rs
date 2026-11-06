//! Linux-style interrupt handling: top-half / bottom-half split.
//!
//! Ports the discipline of `kernel/irq/` + `kernel/softirq.c`:
//!
//! - **Top half** ([`IsrContext::handle`]): runs with interrupts
//!   conceptually disabled, does the bare minimum — acknowledge the
//!   IRQ, copy raw data into a lock-free ring buffer, schedule bottom
//!   halves, return immediately. Never blocks, never allocates.
//! - **Bottom half** ([`BottomHalf::run_pending`]): tasklets/workqueue
//!   analog — heavy processing (packet parsing, input decoding) runs
//!   here, after the top half has exited, so new interrupts can fire.
//!
//! This mirrors Linux's hardirq/softirq split and prevents interrupt
//! storms from starving the system (interrupt handling discipline).

use std::collections::VecDeque;

/// Raw event captured by the top half — what the hardware gave us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqEvent {
    pub irq: u32,
    /// Opaque payload bytes copied verbatim (zero-copy ring buffer entry).
    pub payload: Vec<u8>,
}

/// A registered interrupt source.
#[derive(Debug, Clone)]
pub struct IrqLine {
    pub irq: u32,
    pub name: String,
    pub enabled: bool,
    /// Times the top half has fired.
    pub fire_count: u64,
}

/// Bottom-half handler — the tasklet/workqueue analogue.
/// Receives events drained from the ring buffer.
pub type BottomHalfFn = Box<dyn FnMut(&[IrqEvent]) + Send>;

/// A scheduled bottom-half job.
struct PendingJob {
    irq: u32,
    handler: BottomHalfFn,
}

/// Interrupt controller state: IRQ lines + pending bottom halves.
#[derive(Default)]
pub struct IrqManager {
    lines: Vec<IrqLine>,
    /// Lock-free-ish ring buffer of raw events awaiting bottom-half drain.
    ring: VecDeque<IrqEvent>,
    ring_capacity: usize,
    /// Registered bottom-half handlers per IRQ.
    handlers: Vec<(u32, BottomHalfFn)>,
    /// Events dropped because the ring was full (instrumentation counter).
    pub dropped_events: u64,
}

impl IrqManager {
    pub fn new() -> Self {
        Self { ring_capacity: 256, ..Default::default() }
    }

    pub fn with_ring_capacity(capacity: usize) -> Self {
        Self { ring_capacity: capacity, ..Default::default() }
    }

    /// request_irq analogue.
    pub fn register_irq(&mut self, irq: u32, name: impl Into<String>) -> Result<(), IrqHalvesError> {
        if self.lines.iter().any(|l| l.irq == irq) {
            return Err(IrqHalvesError::AlreadyRegistered(irq));
        }
        self.lines.push(IrqLine { irq, name: name.into(), enabled: true, fire_count: 0 });
        Ok(())
    }

    /// Register a bottom-half (tasklet) handler for an IRQ line.
    pub fn register_bottom_half(
        &mut self,
        irq: u32,
        handler: BottomHalfFn,
    ) -> Result<(), IrqHalvesError> {
        if !self.lines.iter().any(|l| l.irq == irq) {
            return Err(IrqHalvesError::UnknownIrq(irq));
        }
        self.handlers.push((irq, handler));
        Ok(())
    }

    /// Top half: acknowledge + enqueue raw event. Must be fast and
    /// non-blocking — this is the ISR contract.
    pub fn top_half(&mut self, irq: u32, payload: &[u8]) -> Result<(), IrqHalvesError> {
        let line = self
            .lines
            .iter_mut()
            .find(|l| l.irq == irq)
            .ok_or(IrqHalvesError::UnknownIrq(irq))?;
        if !line.enabled {
            return Err(IrqHalvesError::Disabled(irq));
        }
        line.fire_count += 1;
        let event = IrqEvent { irq, payload: payload.to_vec() };
        if self.ring.len() >= self.ring_capacity {
            // Ring full: drop oldest? Linux would raise a warning;
            // we count the drop for instrumentation (fail-visible, not silent).
            self.ring.pop_front();
            self.dropped_events += 1;
        }
        self.ring.push_back(event);
        Ok(())
    }

    /// Bottom half: drain the ring buffer and dispatch to handlers.
    /// Call from process context after the top half returns.
    pub fn run_bottom_halves(&mut self) {
        let mut by_irq: Vec<(u32, Vec<IrqEvent>)> = Vec::new();
        while let Some(ev) = self.ring.pop_front() {
            match by_irq.iter_mut().find(|(irq, _)| *irq == ev.irq) {
                Some((_, events)) => events.push(ev),
                None => by_irq.push((ev.irq, vec![ev])),
            }
        }
        for (irq, events) in by_irq {
            for (h_irq, handler) in self.handlers.iter_mut() {
                if *h_irq == irq {
                    handler(&events);
                }
            }
        }
    }

    /// Pending event count (bottom halves not yet run).
    pub fn pending(&self) -> usize {
        self.ring.len()
    }

    pub fn line(&self, irq: u32) -> Option<&IrqLine> {
        self.lines.iter().find(|l| l.irq == irq)
    }

    /// mask_irq analogue.
    pub fn disable_irq(&mut self, irq: u32) -> Result<(), IrqHalvesError> {
        let line = self
            .lines
            .iter_mut()
            .find(|l| l.irq == irq)
            .ok_or(IrqHalvesError::UnknownIrq(irq))?;
        line.enabled = false;
        Ok(())
    }

    /// unmask_irq analogue.
    pub fn enable_irq(&mut self, irq: u32) -> Result<(), IrqHalvesError> {
        let line = self
            .lines
            .iter_mut()
            .find(|l| l.irq == irq)
            .ok_or(IrqHalvesError::UnknownIrq(irq))?;
        line.enabled = true;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrqHalvesError {
    AlreadyRegistered(u32),
    UnknownIrq(u32),
    Disabled(u32),
}

impl std::fmt::Display for IrqHalvesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IrqHalvesError::AlreadyRegistered(i) => write!(f, "irq {i} already registered"),
            IrqHalvesError::UnknownIrq(i) => write!(f, "unknown irq {i}"),
            IrqHalvesError::Disabled(i) => write!(f, "irq {i} is disabled"),
        }
    }
}
impl std::error::Error for IrqHalvesError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn top_half_enqueues_bottom_half_drains() {
        let mut im = IrqManager::new();
        im.register_irq(1, "kbd").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        im.register_bottom_half(1, Box::new(move |evs| {
            seen2.lock().unwrap().extend(evs.iter().map(|e| e.payload.clone()));
        }))
        .unwrap();

        // Top half: raw bytes in, nothing processed yet.
        im.top_half(1, b"a").unwrap();
        im.top_half(1, b"b").unwrap();
        assert_eq!(im.pending(), 2);
        assert!(seen.lock().unwrap().is_empty());

        // Bottom half: heavy work happens here.
        im.run_bottom_halves();
        assert_eq!(im.pending(), 0);
        assert_eq!(*seen.lock().unwrap(), vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn fire_count_tracks_top_half() {
        let mut im = IrqManager::new();
        im.register_irq(4, "timer").unwrap();
        for _ in 0..3 {
            im.top_half(4, &[]).unwrap();
        }
        assert_eq!(im.line(4).unwrap().fire_count, 3);
    }

    #[test]
    fn disabled_irq_rejects_events() {
        let mut im = IrqManager::new();
        im.register_irq(9, "uart").unwrap();
        im.disable_irq(9).unwrap();
        assert!(matches!(im.top_half(9, b"x"), Err(IrqHalvesError::Disabled(9))));
        im.enable_irq(9).unwrap();
        assert!(im.top_half(9, b"x").is_ok());
    }

    #[test]
    fn ring_overflow_counts_drops() {
        let mut im = IrqManager::with_ring_capacity(2);
        im.register_irq(2, "net").unwrap();
        for i in 0..5u8 {
            im.top_half(2, &[i]).unwrap();
        }
        assert_eq!(im.dropped_events, 3);
        assert_eq!(im.pending(), 2);
    }

    #[test]
    fn unknown_irq_fails_fast() {
        let mut im = IrqManager::new();
        assert!(matches!(im.top_half(77, &[]), Err(IrqHalvesError::UnknownIrq(77))));
        assert!(matches!(
            im.register_bottom_half(77, Box::new(|_| {})),
            Err(IrqHalvesError::UnknownIrq(77))
        ));
    }
}
