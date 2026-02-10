use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// This function allows to "loan" a value in a simple way for multithread environment.
/// The value is embedded in a `Loan<T>` that may be sent to another thread.
/// The thread can then reclaim it back by calling the method `reclaim` on the `Reclaim<T>` struct.
/// The reclamation will be over when the `Loan<T>` is dropped.
pub fn loan<T: std::default::Default>(value: T) -> (Loan<T>, Reclaim<T>) {
    let (tx, rx) = oneshot::channel();
    (
        Loan {
            value,
            tx: Some(tx),
        },
        Reclaim { rx },
    )
}

pub struct Loan<T: std::default::Default> {
    value: T,
    tx: Option<oneshot::Sender<T>>,
}

pub struct Reclaim<T> {
    rx: oneshot::Receiver<T>,
}

impl<T> Reclaim<T> {
    pub fn blocking_reclaim(self) -> T {
        self.rx.blocking_recv().unwrap()
    }
}

impl<T: std::default::Default> std::ops::Drop for Loan<T> {
    fn drop(&mut self) {
        let _ = self
            .tx
            .take()
            .unwrap()
            .send(std::mem::take(&mut self.value));
    }
}

impl<T: std::default::Default> std::ops::Deref for Loan<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: std::default::Default> std::ops::DerefMut for Loan<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

/// Creates an "ordered" channel which allows to send out-of-order values
/// that will be read in order on the receiver side.
/// Senders will block until their value has been effectively received.
/// Senders will actually wait to send their value that a reader is asking a read. This has the nice guarantee that
/// the channel buffer is "always" empty.
/// It is useful for sequencing multithread operations whose results must be processed
/// in a sequence. This has some performance penalty because some threads may wait but it avoid having to store
/// aggregated values before being read. At most, the amount of stored values will be bounded to the number of active senders
pub fn ordered_channel<T, I>() -> (OrderedSender<T, I>, OrderedReceiver<T, I>) {
    let (tx, rx) = mpsc::channel(1);
    let state = Arc::new(Mutex::new(SharedState {
        wanted_idx: None,
        recv_closed: false,
        receiving: false,
    }));
    let cond_var = Arc::new(Condvar::new());
    (
        OrderedSender {
            inner: tx,
            cond_var: Arc::clone(&cond_var),
            state: Arc::clone(&state),
        },
        OrderedReceiver {
            inner: rx,
            cond_var,
            state,
            #[cfg(test)]
            infinite_recv: false,
        },
    )
}

/// Sender side of the channel. It is expected to be used in a synchronous environment as it will block until its value has been read.
pub struct OrderedSender<T, I = usize> {
    inner: mpsc::Sender<T>,
    cond_var: Arc<Condvar>,
    state: Arc<Mutex<SharedState<I>>>,
}

/// Receiver side of the channel.
pub struct OrderedReceiver<T, I = usize> {
    inner: mpsc::Receiver<T>,
    cond_var: Arc<Condvar>,
    state: Arc<Mutex<SharedState<I>>>,
    #[cfg(test)]
    infinite_recv: bool,
}

struct SharedState<I> {
    wanted_idx: Option<I>,
    recv_closed: bool,
    receiving: bool,
}

impl<T, I: num::Num + Copy> OrderedSender<T, I> {
    /// Send the given `value` at the index `idx`.
    /// This will block until the value is (about to be) read or an error occurs.
    /// This returns an error if the other side has been closed.
    pub fn send(&mut self, idx: I, value: T) -> Result<(), anyhow::Error> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.recv_closed {
                anyhow::bail!("Receiver has been closed");
            } else {
                let can_send = state.wanted_idx == Some(idx);

                if can_send {
                    return self
                        .inner
                        .blocking_send(value)
                        .map_err(|_| anyhow::anyhow!("Receiver has been closed"));
                } else {
                    state = self.cond_var.wait(state).unwrap();
                }
            }
        }
    }

    /// Close the channel from the sender side, unblocking sibling senders stuck in `send()`.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.recv_closed = true;
        self.cond_var.notify_all();
    }

    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    #[cfg(test)]
    pub fn max_capacity(&self) -> usize {
        self.inner.max_capacity()
    }
}

impl<T, I: num::Num + Copy> Clone for OrderedSender<T, I> {
    fn clone(&self) -> Self {
        OrderedSender {
            inner: self.inner.clone(),
            cond_var: self.cond_var.clone(),
            state: self.state.clone(),
        }
    }
}

impl<T, I: num::Num + Copy> OrderedReceiver<T, I> {
    /// Receive the value at the next index on this channel or None if channel is closed or all sender have been dropped
    /// This function is cancel safe.
    pub async fn recv(&mut self) -> Option<T> {
        {
            let mut state = self.state.lock().unwrap();
            if !state.receiving {
                state.wanted_idx = match state.wanted_idx {
                    None => Some(I::zero()),
                    Some(i) => Some(i.add(I::one())),
                };
                state.receiving = true;
                self.cond_var.notify_all();
            }
        }

        #[cfg(test)]
        if self.infinite_recv {
            futures::future::pending::<()>().await;
        }

        let value = self.inner.recv().await?;

        let mut state = self.state.lock().unwrap();
        state.receiving = false;

        Some(value)
    }

    #[cfg(test)]
    fn set_infinite_recv(&mut self, infinite: bool) {
        self.infinite_recv = infinite;
    }
}

impl<T, I> OrderedReceiver<T, I> {
    /// Close this receiver
    pub fn close(&mut self) {
        let mut state = self.state.lock().unwrap();
        if !state.recv_closed {
            self.inner.close();
            state.recv_closed = true;
            self.cond_var.notify_all();
        }
    }
}

impl<T, I> std::ops::Drop for OrderedReceiver<T, I> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn ordered() {
        let (tx, mut rx) = super::ordered_channel();
        {
            let mut tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(2, "third");
            });
        }
        {
            let mut tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(1, "second");
            });
        }
        {
            let mut tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(0, "first");
                let _ = tx.send(3, "fourth");
            });
        }
        std::mem::drop(tx);

        assert_eq!(rx.recv().await, Some("first"));
        assert_eq!(rx.recv().await, Some("second"));
        assert_eq!(rx.recv().await, Some("third"));
        assert_eq!(rx.recv().await, Some("fourth"));
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn dropping_rx() {
        let (mut tx, rx) = super::ordered_channel();
        let handle = std::thread::spawn(move || {
            let res = tx.send(10, "");
            (res.is_err(), tx.capacity() == tx.max_capacity()) // channel must be empty if no read was attempted
        });
        std::mem::drop(rx);

        if let Ok((is_err, full_capacity)) = handle.join() {
            assert!(is_err);
            assert!(full_capacity);
        } else {
            panic!("Dropping the receiver should cause an error in send")
        }
    }

    #[tokio::test]
    async fn cancel_safe() {
        let (mut tx, mut rx) = super::ordered_channel();

        std::thread::spawn(move || {
            let _ = tx.send(0, "first");
            let _ = tx.send(1, "second");
        });

        rx.set_infinite_recv(true);
        tokio::select! {
            _ = rx.recv() => { // will be canceled and must not have consumed an element
                panic!("should not happen, broken test");
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
            }
        }

        rx.set_infinite_recv(false);
        assert_eq!(rx.recv().await, Some("first"));
        assert_eq!(rx.recv().await, Some("second"));
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn no_deadlock_if_recv_canceled() {
        let (mut tx, mut rx) = super::ordered_channel();

        let handle = std::thread::spawn(move || {
            let _ = tx.send(0, "first").unwrap();
            let res = tx.send(1, "second");
            println!("{:?}", res);
            (res.is_err(), tx.capacity() == tx.max_capacity())
        });

        assert!(rx.recv().await.is_some()); // read first to ensure we are in the process of sending

        rx.set_infinite_recv(true);
        tokio::select! {
            _ = rx.recv() => {
                panic!("should not happen, broken test");
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
            }
        }

        std::mem::drop(rx);
        let _ = handle.join();
    }
}
