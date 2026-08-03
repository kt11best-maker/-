use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

/// 接続状態（キルスイッチ・監視用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Reconnecting { attempts: u32 },
    Disconnected,
}

impl ConnectionStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionStatus::Connected)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Reconnecting { .. } => "reconnecting",
            ConnectionStatus::Disconnected => "disconnected",
        }
    }
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionStatus::Reconnecting { attempts } => {
                write!(f, "reconnecting(attempts={attempts})")
            }
            other => f.write_str(other.as_str()),
        }
    }
}

const DISCONNECTED: u8 = 0;
const CONNECTED: u8 = 1;
const RECONNECTING: u8 = 2;

/// スレッド間で共有される接続状態。
///
/// 監視タスクが別スレッドから読むため、ロックを持たない atomic 実装にしている
/// （WS 受信ループがロック待ちで詰まる余地を作らない）。
#[derive(Debug)]
pub struct ConnectionState {
    code: AtomicU8,
    attempts: AtomicU32,
}

impl Default for ConnectionState {
    fn default() -> Self {
        ConnectionState {
            code: AtomicU8::new(DISCONNECTED),
            attempts: AtomicU32::new(0),
        }
    }
}

impl ConnectionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_connected(&self) {
        self.attempts.store(0, Ordering::Relaxed);
        self.code.store(CONNECTED, Ordering::Release);
    }

    pub fn set_reconnecting(&self, attempts: u32) {
        self.attempts.store(attempts, Ordering::Relaxed);
        self.code.store(RECONNECTING, Ordering::Release);
    }

    pub fn set_disconnected(&self) {
        self.code.store(DISCONNECTED, Ordering::Release);
    }

    pub fn get(&self) -> ConnectionStatus {
        match self.code.load(Ordering::Acquire) {
            CONNECTED => ConnectionStatus::Connected,
            RECONNECTING => ConnectionStatus::Reconnecting {
                attempts: self.attempts.load(Ordering::Relaxed),
            },
            _ => ConnectionStatus::Disconnected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        let s = ConnectionState::new();
        assert_eq!(s.get(), ConnectionStatus::Disconnected);

        s.set_reconnecting(3);
        assert_eq!(s.get(), ConnectionStatus::Reconnecting { attempts: 3 });

        // 接続成功で試行回数はリセットされる
        s.set_connected();
        assert_eq!(s.get(), ConnectionStatus::Connected);
        s.set_reconnecting(1);
        assert_eq!(s.get(), ConnectionStatus::Reconnecting { attempts: 1 });

        s.set_disconnected();
        assert_eq!(s.get(), ConnectionStatus::Disconnected);
    }
}
