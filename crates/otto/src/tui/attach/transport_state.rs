//! Connection ownership and the single in-flight attached-session request.

use super::*;

// StatusStream owns its transport buffers; keeping it inline avoids another allocation.
#[allow(clippy::large_enum_variant)]
pub(super) enum ConnectionState {
    Connected(StatusStream),
    Disconnected,
}

impl ConnectionState {
    pub(super) fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }

    pub(super) fn status_mut(&mut self) -> Option<&mut StatusStream> {
        match self {
            Self::Connected(status) => Some(status),
            Self::Disconnected => None,
        }
    }
}

pub(super) enum TransportState<'a> {
    Idle,
    Reading(PendingTransport<'a>),
    Sending {
        pending: PendingTransport<'a>,
        cancel_after_send: bool,
    },
    Reconnecting(PendingTransport<'a>),
    Canceling(PendingTransport<'a>),
}

impl<'a> TransportState<'a> {
    pub(super) fn pending_mut(&mut self) -> Option<&mut PendingTransport<'a>> {
        match self {
            Self::Idle => None,
            Self::Reading(pending)
            | Self::Sending { pending, .. }
            | Self::Reconnecting(pending)
            | Self::Canceling(pending) => Some(pending),
        }
    }

    pub(super) fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub(super) fn is_owned(&self) -> bool {
        matches!(self, Self::Sending { .. } | Self::Canceling(_))
    }

    pub(super) fn discard_read(&mut self) -> bool {
        if matches!(self, Self::Reading(_) | Self::Reconnecting(_)) {
            *self = Self::Idle;
            true
        } else {
            false
        }
    }

    /// A repeated cancel while cancellation is pending already has an owner.
    pub(super) fn defer_cancel(&mut self) -> bool {
        match self {
            Self::Sending {
                cancel_after_send, ..
            } => {
                *cancel_after_send = true;
                true
            }
            Self::Canceling(_) => true,
            _ => false,
        }
    }
}

impl<'a> Ui<'a> {
    pub(super) fn capture_call(&self, call: TransportCall<'a>) -> PendingTransport<'a> {
        PendingTransport {
            session: self.remote.session(),
            epoch: self.epoch,
            transcript: self.transcript,
            call,
        }
    }

    pub(super) fn start_read(&mut self, call: TransportCall<'a>) {
        if self.transport.is_idle() {
            self.transport = TransportState::Reading(self.capture_call(call));
        }
    }

    pub(super) fn start_reconnect(&mut self, call: TransportCall<'a>) {
        if self.transport.is_idle() {
            self.transport = TransportState::Reconnecting(self.capture_call(call));
        }
    }

    pub(super) fn start_send(&mut self, call: TransportCall<'a>) {
        if !self.transport.is_owned() {
            self.transport = TransportState::Sending {
                pending: self.capture_call(call),
                cancel_after_send: false,
            };
        }
    }

    pub(super) fn start_cancel(&mut self, call: TransportCall<'a>) {
        if !self.transport.is_owned() {
            self.transport = TransportState::Canceling(self.capture_call(call));
        }
    }

    pub(super) fn mark_connected(&mut self, status: StatusStream) {
        self.connection = ConnectionState::Connected(status);
        self.app.connected = true;
    }
}
