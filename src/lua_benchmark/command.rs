//! Commands a Lua handler can emit, and the bounded sink that records them.
//!
//! The benchmark command sink does not perform full real actions in every
//! synthetic scenario (per the mission) — it always records command type,
//! target bot, source event, and enqueue timestamp. The full-runtime
//! scenario additionally routes selected commands through the real
//! [`crate::core::supervisor::SupervisorHandle`] (see `full_runtime.rs`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Instant;

use super::event::BotId;

/// A small, realistic command surface — every variant maps directly onto a
/// real `SupervisorHandle`/`BotCommand` action (see `docs/lua_design.md`),
/// deliberately not the full action API: this benchmark measures dispatch
/// architecture, not action coverage.
#[derive(Debug, Clone, PartialEq)]
pub enum BenchCommand {
    Forward(bool),
    Chat(String),
    ClickSlot { raw_slot: u16, right_click: bool },
}

impl BenchCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Forward(_) => "forward",
            Self::Chat(_) => "chat",
            Self::ClickSlot { .. } => "click_slot",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandRecord {
    pub bot_id: BotId,
    pub command: BenchCommand,
    pub source_event: &'static str,
    pub enqueued_at: Instant,
    pub seq: u64,
}

/// A bounded sink Lua's `bot.command(...)` calls write into. Full sink =
/// dropped command, counted, never blocking the calling handler (a script
/// emitting commands faster than the sink drains must never be able to
/// stall Lua execution, matching "do not allow Lua to block core Minecraft
/// processing").
#[derive(Clone)]
pub struct CommandSink {
    tx: SyncSender<CommandRecord>,
    seq: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

pub struct CommandSinkStats {
    pub emitted: u64,
    pub dropped: u64,
}

impl CommandSink {
    pub fn bounded(capacity: usize) -> (Self, Receiver<CommandRecord>) {
        let (tx, rx) = sync_channel(capacity);
        (
            Self {
                tx,
                seq: Arc::new(AtomicU64::new(0)),
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    pub fn emit(&self, bot_id: BotId, command: BenchCommand, source_event: &'static str) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let record = CommandRecord {
            bot_id,
            command,
            source_event,
            enqueued_at: Instant::now(),
            seq,
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = self.tx.try_send(record)
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> CommandSinkStats {
        CommandSinkStats {
            emitted: self.seq.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitted_commands_are_received_in_order_with_metadata_intact() {
        let (sink, rx) = CommandSink::bounded(8);
        sink.emit(BotId(1), BenchCommand::Forward(true), "chat");
        sink.emit(BotId(1), BenchCommand::Chat("hi".into()), "chat");
        let first = rx.recv().unwrap();
        assert_eq!(first.bot_id, BotId(1));
        assert_eq!(first.command, BenchCommand::Forward(true));
        assert_eq!(first.source_event, "chat");
        assert_eq!(first.seq, 0);
        let second = rx.recv().unwrap();
        assert_eq!(second.seq, 1);
    }

    #[test]
    fn a_full_sink_drops_and_counts_rather_than_blocking() {
        let (sink, _rx) = CommandSink::bounded(1);
        sink.emit(BotId(0), BenchCommand::Forward(true), "x");
        sink.emit(BotId(0), BenchCommand::Forward(false), "x");
        sink.emit(BotId(0), BenchCommand::Forward(true), "x");
        let stats = sink.stats();
        assert_eq!(stats.emitted, 3);
        assert!(stats.dropped >= 2);
    }
}
