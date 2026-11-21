//! portal scan queue, keeps count of the waiting requests to tell users their position

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::mpsc;

use crate::ScanRequest;

/// sending side of the scan queue, cheap to clone
#[derive(Clone, Debug)]
pub struct ScanQueue {
    tx: mpsc::UnboundedSender<ScanRequest>,
    pending: Arc<AtomicUsize>,
}

/// receiving side of the scan queue, used by the portal scanner
#[derive(Debug)]
pub struct ScanReceiver {
    rx: mpsc::UnboundedReceiver<ScanRequest>,
    pending: Arc<AtomicUsize>,
}

pub fn scan_queue() -> (ScanQueue, ScanReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let pending = Arc::new(AtomicUsize::new(0));
    (ScanQueue { tx, pending: Arc::clone(&pending) }, ScanReceiver { rx, pending })
}

impl ScanQueue {
    /// queues the request, returns its position in the queue: 1 is the next one to be processed
    pub fn send(&self, request: ScanRequest) -> Result<usize, mpsc::error::SendError<ScanRequest>> {
        // counted before sending, so the receiver can't decrement it first
        let position = self.pending.fetch_add(1, Ordering::SeqCst) + 1;
        self.tx.send(request).map(|()| position).inspect_err(|_| {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        })
    }
}

impl ScanReceiver {
    /// the next request, if there's one already waiting
    pub fn try_recv(&mut self) -> Option<ScanRequest> {
        let request = self.rx.try_recv().ok()?;
        self.pending.fetch_sub(1, Ordering::SeqCst);
        Some(request)
    }

    pub async fn recv(&mut self) -> Option<ScanRequest> {
        let request = self.rx.recv().await?;
        self.pending.fetch_sub(1, Ordering::SeqCst);
        Some(request)
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::*;
    use crate::PortalOrCoords;

    fn request(id: &str) -> ScanRequest {
        (PortalOrCoords::Portal { id: SmolStr::new(id) }, 0, None)
    }

    #[tokio::test]
    async fn positions() {
        let (queue, mut receiver) = scan_queue();
        let other = queue.clone();
        assert_eq!(queue.send(request("a")).unwrap(), 1);
        assert_eq!(other.send(request("b")).unwrap(), 2);
        assert_eq!(queue.send(request("c")).unwrap(), 3);

        let Some((PortalOrCoords::Portal { id }, ..)) = receiver.recv().await else { panic!() };
        assert_eq!(id, "a");
        // "b" and "c" are still waiting
        assert_eq!(queue.send(request("d")).unwrap(), 3);

        for _ in 0..3 {
            receiver.recv().await.unwrap();
        }
        assert_eq!(queue.send(request("e")).unwrap(), 1);

        assert!(receiver.try_recv().is_some());
        assert!(receiver.try_recv().is_none());
        assert_eq!(queue.send(request("f")).unwrap(), 1);
        receiver.recv().await.unwrap();

        drop(receiver);
        assert!(queue.send(request("g")).is_err());
        // a failed send doesn't take a place
        assert_eq!(queue.pending.load(Ordering::SeqCst), 0);
    }
}
