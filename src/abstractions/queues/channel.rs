use std::any::Any;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use abstractions::futures::future::Future;
use abstractions::poll::{Poll, Async};
use abstractions::queues::slot::{Slot, Token};
use abstractions::streams::stream::Stream;
use abstractions::tasks::task;

pub fn create<T, E>() -> (Sender<T, E>, Receiver<T, E>) {
    let inner = Arc::new(Inner {
        slot: Slot::new(None),
        receiver_gone: AtomicBool::new(false),
    });
    let sender = Sender {
        inner: inner.clone(),
    };
    let receiver = Receiver {
        inner: inner,
        on_full_token: None,
    };
    (sender, receiver)
}

/// The transmission end of a channel which is used to send values.
///
/// This is created by the `channel` method in the `stream` module.
pub struct Sender<T, E> {
    inner: Arc<Inner<T, E>>,
}

/// A future returned by the `Sender::send` method which will resolve to the
/// sender once it's available to send another message.
#[must_use = "futures do nothing unless polled"]
pub struct FutureSender<T, E> {
    sender: Option<Sender<T, E>>,
    data: Option<Result<T, E>>,
    on_empty_token: Option<Token>,
}

/// The receiving end of a channel which implements the `Stream` trait.
///
/// This is a concrete implementation of a stream which can be used to represent
/// a stream of values being computed elsewhere. This is created by the
/// `channel` method in the `stream` module.
#[must_use = "streams do nothing unless polled"]
pub struct Receiver<T, E> {
    inner: Arc<Inner<T, E>>,
    on_full_token: Option<Token>,
}

struct Inner<T, E> {
    slot: Slot<Message<Result<T, E>>>,
    receiver_gone: AtomicBool,
}

enum Message<T> {
    Data(T),
    Done,
}

/// Error type returned by `FutureSender` when the receiving end of a `channel` is dropped
pub struct SendError<T, E>(Result<T, E>);

impl<T, E> fmt::Debug for SendError<T, E> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("SendError")
            .field(&"...")
            .finish()
    }
}

impl<T, E> fmt::Display for SendError<T, E> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "send failed because receiver is gone")
    }
}

impl<T, E> Error for SendError<T, E>
    where T: Any, E: Any
{
    fn description(&self) -> &str {
        "send failed because receiver is gone"
    }
}


impl<T, E> Stream for Receiver<T, E> {
    type Item = T;
    type Error = E;

    fn poll(&mut self) -> Poll<Option<T>, E> {
        if let Some(token) = self.on_full_token.take() {
            self.inner.slot.cancel(token);
        }

        match self.inner.slot.try_consume() {
            Ok(Message::Data(Ok(e))) => Ok(Async::Ready(Some(e))),
            Ok(Message::Data(Err(e))) => Err(e),
            Ok(Message::Done) => Ok(Async::Ready(None)),
            Err(..) => {
                let task = task::park();
                self.on_full_token = Some(self.inner.slot.on_full(move |_| {
                    task.unpark();
                }));
                Ok(Async::NotReady)
            }
        }
    }
}

impl<T, E> Drop for Receiver<T, E> {
    fn drop(&mut self) {
        self.inner.receiver_gone.store(true, Ordering::SeqCst);
        if let Some(token) = self.on_full_token.take() {
            self.inner.slot.cancel(token);
        }
        self.inner.slot.on_full(|slot| {
            drop(slot.try_consume());
        });
    }
}

impl<T, E> Sender<T, E> {
    pub fn send(self, t: Result<T, E>) -> FutureSender<T, E> {
        FutureSender {
            sender: Some(self),
            data: Some(t),
            on_empty_token: None,
        }
    }
}

impl<T, E> Drop for Sender<T, E> {
    fn drop(&mut self) {
        self.inner.slot.on_empty(None, |slot, _none| {
            slot.try_produce(Message::Done).ok().unwrap();
        });
    }
}

impl<T, E> Future for FutureSender<T, E> {
    type Item = Sender<T, E>;
    type Error = SendError<T, E>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let data = self.data.take().expect("cannot poll FutureSender twice");
        let sender = self.sender.take().expect("cannot poll FutureSender twice");
        if let Some(token) = self.on_empty_token.take() {
            sender.inner.slot.cancel(token);
        }
        if sender.inner.receiver_gone.load(Ordering::SeqCst) {
            return Err(SendError(data))
        }
        match sender.inner.slot.try_produce(Message::Data(data)) {
            Ok(()) => Ok(Async::Ready(sender)),
            Err(e) => {
                let task = task::park();
                let token = sender.inner.slot.on_empty(None, move |_slot, _item| {
                    task.unpark();
                });
                self.on_empty_token = Some(token);
                self.data = Some(match e.into_inner() {
                    Message::Data(data) => data,
                    Message::Done => panic!(),
                });
                self.sender = Some(sender);
                Ok(Async::NotReady)
            }
        }
    }
}

impl<T, E> Drop for FutureSender<T, E> {
    fn drop(&mut self) {
        if let Some(token) = self.on_empty_token.take() {
            if let Some(sender) = self.sender.take() {
                sender.inner.slot.cancel(token);
            }
        }
    }
}
