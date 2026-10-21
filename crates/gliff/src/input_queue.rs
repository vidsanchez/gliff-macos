//! A genuinely bounded per-connection FIFO. Normal traffic stops at 128;
//! remaining slots are shared by key/mouse releases, axis-stop, Bye and
//! clipboard control. Never wait for network space. Essential overflow poisons
//! the connection and wakes its worker independently of the input backlog.
use crate::clipboard::ToWorker;
use gliff_proto::ClientMsg;
use tokio::sync::mpsc;
pub const NORMAL_LIMIT: usize = 128;
const CAPACITY: usize = 256;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct Failure(Arc<FailureState>);
#[derive(Default)]
struct FailureState {
    failed: AtomicBool,
    notify: Notify,
}
impl Failure {
    fn fail(&self) {
        if !self.0.failed.swap(true, Ordering::AcqRel) {
            tracing::warn!("Essential input/control queue exhausted; closing connection");
            self.0.notify.notify_waiters();
        }
    }
    pub fn is_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    pub async fn wait(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_failed() {
                return;
            }
            notified.await;
        }
    }
}
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::Sender<ToWorker>,
    failure: Failure,
}
#[derive(Clone)]
pub struct WeakSender {
    tx: mpsc::WeakSender<ToWorker>,
    failure: Failure,
}
pub struct Receiver {
    rx: mpsc::Receiver<ToWorker>,
    failure: Failure,
}
pub fn channel() -> (Sender, Receiver) {
    let (tx, rx) = mpsc::channel(CAPACITY);
    let failure = Failure::default();
    (
        Sender {
            tx,
            failure: failure.clone(),
        },
        Receiver { rx, failure },
    )
}
impl Receiver {
    pub fn failure(&self) -> Failure {
        self.failure.clone()
    }
    pub async fn recv(&mut self) -> Option<ToWorker> {
        tokio::select! { biased;
            _ = self.failure.wait() => None,
            msg = self.rx.recv() => msg,
        }
    }
    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<ToWorker, mpsc::error::TryRecvError> {
        if self.failure.is_failed() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        self.rx.try_recv()
    }
}
impl Sender {
    pub fn send(&self, msg: ToWorker) -> Result<(), mpsc::error::TrySendError<ToWorker>> {
        let droppable = matches!(
            &msg,
            ToWorker::Zoom(_)
                | ToWorker::Send(
                    ClientMsg::PointerMotion { .. } | ClientMsg::PointerAxis { stop: false, .. }
                )
        );
        let reserved = matches!(
            &msg,
            ToWorker::CancelTransfer(_)
                | ToWorker::LocalOffer { .. }
                | ToWorker::FetchText { .. }
                | ToWorker::Send(
                    ClientMsg::Key { pressed: false, .. }
                        | ClientMsg::PointerButton { pressed: false, .. }
                        | ClientMsg::PointerAxis { stop: true, .. }
                        | ClientMsg::Bye
                )
        );
        if self.is_closed() {
            return Err(mpsc::error::TrySendError::Closed(msg));
        }
        if !reserved && self.len() >= NORMAL_LIMIT {
            if !droppable {
                self.failure.fail();
            }
            return Err(mpsc::error::TrySendError::Full(msg));
        }
        match self.tx.try_send(msg) {
            Err(mpsc::error::TrySendError::Full(msg)) => {
                if !droppable {
                    self.failure.fail();
                }
                Err(mpsc::error::TrySendError::Full(msg))
            }
            result => result,
        }
    }
    pub fn len(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }
    pub fn is_closed(&self) -> bool {
        self.failure.is_failed() || self.tx.is_closed()
    }
    pub fn same_channel(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
    pub fn downgrade(&self) -> WeakSender {
        WeakSender {
            tx: self.tx.downgrade(),
            failure: self.failure.clone(),
        }
    }
}
impl WeakSender {
    pub fn upgrade(&self) -> Option<Sender> {
        if self.failure.is_failed() {
            return None;
        }
        self.tx.upgrade().map(|tx| Sender {
            tx,
            failure: self.failure.clone(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_release_and_clipboard_control_survive_normal_pressure_in_fifo_order() {
        let (tx, mut rx) = channel();
        tx.send(ToWorker::Send(ClientMsg::PointerButton {
            button: 272,
            pressed: true,
        }))
        .unwrap();
        for _ in 1..NORMAL_LIMIT {
            tx.send(ToWorker::Send(ClientMsg::PointerMotion { x: 1.0, y: 2.0 }))
                .unwrap();
        }
        assert!(tx.send(ToWorker::Zoom(1)).is_err());
        tx.send(ToWorker::Send(ClientMsg::PointerButton {
            button: 272,
            pressed: false,
        }))
        .unwrap();
        tx.send(ToWorker::CancelTransfer(7)).unwrap();
        tx.send(ToWorker::LocalOffer {
            mime_types: vec![],
            files: Default::default(),
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::PointerButton { pressed: true, .. })
        ));
        for _ in 1..NORMAL_LIMIT {
            assert!(matches!(
                rx.try_recv().unwrap(),
                ToWorker::Send(ClientMsg::PointerMotion { .. })
            ));
        }
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::PointerButton { pressed: false, .. })
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::CancelTransfer(7)
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::LocalOffer { .. }
        ));
    }
    #[test]
    fn essential_overflow_notifies_consumer_without_draining_backlog() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = channel();
            for _ in 0..CAPACITY {
                tx.send(ToWorker::Send(ClientMsg::Key {
                    keycode: 30,
                    pressed: false,
                }))
                .unwrap();
            }
            assert!(tx.send(ToWorker::CancelTransfer(1)).is_err());
            assert!(tx.is_closed());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(tx
                .send(ToWorker::LocalOffer {
                    mime_types: vec![],
                    files: Default::default()
                })
                .is_err());
            assert!(tx.downgrade().upgrade().is_none());
        });
    }
    #[test]
    fn clipboard_weak_sender_clones_without_retaining_connection() {
        let (tx, mut rx) = channel();
        let weak = tx.downgrade();
        let copy = weak.clone();
        copy.upgrade().unwrap().send(ToWorker::Zoom(2)).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), ToWorker::Zoom(2)));
        drop(tx);
        assert!(weak.upgrade().is_none());
        assert!(copy.upgrade().is_none());
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn slow_consumer_has_real_bound_and_reserved_release_capacity() {
        let (tx, mut rx) = channel();
        for _ in 0..NORMAL_LIMIT {
            tx.send(ToWorker::Zoom(1)).unwrap();
        }
        assert!(tx.send(ToWorker::Zoom(1)).is_err());
        for code in 0..128 {
            tx.send(ToWorker::Send(ClientMsg::Key {
                keycode: code,
                pressed: false,
            }))
            .unwrap();
        }
        assert_eq!(tx.len(), CAPACITY);
        for _ in 0..CAPACITY {
            assert!(rx.try_recv().is_ok());
        }
        assert_eq!(tx.len(), 0);
    }
}
