//! Backpressure with a deadline for the AudioQueue feeders.
//!
//! The queue's callback drains a bounded channel; a writer waits while it is
//! full. When the device stops pulling audio (unplugged, an iOS audio-session
//! interruption) the callback never runs again, so a plain `send` would block
//! the writer, and anything joining its thread, forever.

use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// Sends `item`, waiting while the channel is full, but fails once `stall`
/// passes without the receiver making room.
pub(crate) fn send_or_stall<T>(
    tx: &SyncSender<T>,
    mut item: T,
    stall: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + stall;
    loop {
        match tx.try_send(item) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(back)) => {
                if Instant::now() >= deadline {
                    return Err("audio device stalled: the output stopped draining \
                         (device removed or failed?)"
                        .into());
                }
                item = back;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TrySendError::Disconnected(_)) => return Err("audio channel closed".into()),
        }
    }
}

/// Allow several callback periods at low sample rates while preserving a
/// finite upper bound for malformed or unsupported rates.
pub(crate) fn callback_stall_timeout(frames: u32, rate: u32) -> Duration {
    let four_periods_ms = 4 * u64::from(frames) * 1000 / u64::from(rate.max(1));
    Duration::from_millis(four_periods_ms.clamp(1000, 5000))
}

/// Let queued hardware audio finish, but never turn a bogus sample rate into
/// a multi-hour teardown sleep.
pub(crate) fn buffered_drain_delay(frames: u32, buffers: u32, rate: u32) -> Duration {
    let millis = u64::from(frames)
        .saturating_mul(u64::from(buffers))
        .saturating_mul(1000)
        / u64::from(rate.max(1));
    Duration::from_millis(millis.min(2000) + 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn callback_deadline_scales_without_becoming_unbounded() {
        assert_eq!(callback_stall_timeout(2048, 44100), Duration::from_secs(1));
        assert_eq!(
            callback_stall_timeout(2048, 8000),
            Duration::from_millis(1024)
        );
        assert_eq!(callback_stall_timeout(2048, 1), Duration::from_secs(5));
        assert_eq!(callback_stall_timeout(2048, 0), Duration::from_secs(5));
        assert_eq!(
            buffered_drain_delay(2048, 4, 8000),
            Duration::from_millis(1044)
        );
        assert_eq!(
            buffered_drain_delay(2048, 4, 1),
            Duration::from_millis(2020)
        );
        assert_eq!(
            buffered_drain_delay(2048, 4, 0),
            Duration::from_millis(2020)
        );
    }

    #[test]
    fn waits_for_the_receiver_to_make_room() {
        let (tx, rx) = sync_channel::<u8>(1);
        tx.send(1).unwrap();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            (rx.recv().unwrap(), rx.recv().unwrap())
        });
        send_or_stall(&tx, 2, Duration::from_secs(5)).unwrap();
        assert_eq!(reader.join().unwrap(), (1, 2));
    }

    #[test]
    fn fails_when_nothing_drains() {
        let (tx, _rx) = sync_channel::<u8>(1);
        tx.send(1).unwrap();
        let started = Instant::now();
        let err = send_or_stall(&tx, 2, Duration::from_millis(50)).unwrap_err();
        assert!(err.contains("stalled"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn fails_when_the_receiver_is_gone() {
        let (tx, rx) = sync_channel::<u8>(1);
        drop(rx);
        let err = send_or_stall(&tx, 1, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("closed"), "{err}");
    }
}
