// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

mod bound_counter {
  use crate::{
    ext::future::TaskTracker,
    util::dropkick::{Dropkick, DropkickSync},
  };
  use std::sync::Arc;

  const BASELINE_COUNT: usize = 1; // number of counters present when true count is zero

  pub struct BoundCounter(Dropkick<BoundCounterInner>);

  impl BoundCounter {
    pub fn new(
      notifier: &Arc<tokio::sync::watch::Sender<usize>>,
      counter_holder: &Arc<Arc<()>>,
    ) -> Self {
      let inner = BoundCounterInner::new(notifier, counter_holder);
      Self(Dropkick::new(inner))
    }
  }

  struct BoundCounterInner {
    notifier: Arc<tokio::sync::watch::Sender<usize>>,
    counter_holder: Arc<Arc<()>>,
    count_entry: Arc<()>,
  }

  impl BoundCounterInner {
    pub fn new(
      notifier: &Arc<tokio::sync::watch::Sender<usize>>,
      counter_holder: &Arc<Arc<()>>,
    ) -> Self {
      let this = Self {
        notifier: notifier.clone(),
        count_entry: counter_holder.as_ref().clone(),
        counter_holder: counter_holder.clone(),
      };
      Self::update_current_count(&this.notifier, &this.counter_holder);
      this
    }

    pub fn update_current_count(
      notifier: &tokio::sync::watch::Sender<usize>,
      counter_holder: &Arc<Arc<()>>,
    ) {
      let inner_ref: &Arc<()> = &*counter_holder;
      let current_count = Arc::strong_count(inner_ref) - BASELINE_COUNT;
      notifier.send_replace(current_count);
    }
  }

  impl DropkickSync for BoundCounterInner {
    fn dropkick(self) {
      drop(self.count_entry);
      Self::update_current_count(&self.notifier, &self.counter_holder);
    }
  }

  enum BoundCounterTrackerState {
    /// Tracked task has yet to be polled, so tracking has not yet been established
    Unregistered {
      notifier: Arc<tokio::sync::watch::Sender<usize>>,
      counter_holder: Arc<Arc<()>>,
    },
    /// Tracking is registered; dropping will destroy this registration.
    /// Further calls to "TaskTracker::register" will yield the same registration,
    /// which allows us to safely double-register, which in turn gives us the
    /// option to "pre-register" for when unpolled tasks need tracked as well.
    Registered(BoundCounter),
  }

  impl BoundCounterTrackerState {
    pub fn new(
      notifier: Arc<tokio::sync::watch::Sender<usize>>,
      counter_holder: Arc<Arc<()>>,
    ) -> Self {
      Self::Unregistered {
        notifier,
        counter_holder,
      }
    }
  }

  // Wrap the inner type to keep it private to the module,
  // and to isolate the Drop behaviour constraint surface.
  /// Tracks the count of a task by pushing notifications of the latest outstanding count to a [tokio::sync::watch] channel.
  #[repr(transparent)]
  pub struct BoundCounterTracker(BoundCounterTrackerState);

  impl BoundCounterTracker {
    pub fn new(
      notifier: Arc<tokio::sync::watch::Sender<usize>>,
      counter_holder: Arc<Arc<()>>,
    ) -> Self {
      Self(BoundCounterTrackerState::new(notifier, counter_holder))
    }

    /// Tracks immediately rather than waiting for an initial poll
    pub fn new_preregistered(
      notifier: Arc<tokio::sync::watch::Sender<usize>>,
      counter_holder: Arc<Arc<()>>,
    ) -> Self {
      Self::new(notifier, counter_holder).register_sync()
    }

    fn register_sync(mut self) -> Self {
      match &mut self.0 {
        BoundCounterTrackerState::Registered(_) => self,
        BoundCounterTrackerState::Unregistered {
          notifier,
          counter_holder,
        } => Self(BoundCounterTrackerState::Registered(BoundCounter::new(
          &notifier,
          &counter_holder,
        ))),
      }
    }
  }

  impl TaskTracker for BoundCounterTracker {
    type RegistrationFuture = futures::future::Ready<Self>;

    fn register(self) -> Self::RegistrationFuture {
      futures::future::ready(self.register_sync())
    }
  }

  #[cfg(test)]
  mod tests {
    use std::{sync::Arc, task::Poll, time::Duration};

    use futures::FutureExt;

    use crate::ext::{future::FutureExtExt, stream::bound_counter::BoundCounterTracker};

    #[tokio::test]
    async fn bound_counter_tracking_preregistered() {
      use tokio::sync::{mpsc::unbounded_channel, watch};
      let (watch_tx, mut watch_rx) = watch::channel(Default::default());
      let (updater, counter_holder) = (Arc::new(watch_tx), Arc::new(Arc::new(())));
      let (queue_tx, mut queue_rx) = unbounded_channel();
      const COUNT: usize = 100usize;
      for i in 0..COUNT {
        queue_tx
          .send(BoundCounterTracker::new_preregistered(
            updater.clone(),
            counter_holder.clone(),
          ))
          .ok()
          .expect("tracker send failed");
        assert_eq!(
          *watch_rx.borrow(),
          i + 1,
          "Tracked value was out of sync with allocated trackers in countup"
        );
      }
      drop(queue_tx);
      println!("Count-up complete");
      let mut outstanding = COUNT;
      while let Some(item) = queue_rx.recv().await {
        assert!(outstanding > 0, "Received more than expected?");
        outstanding -= 1;
        let expectation = watch_rx.changed();
        drop(item);
        print!("Waiting for a notification, or 5 seconds... ");
        let reality = expectation
          .map(|_| ())
          .poll_until(tokio::time::sleep(Duration::from_secs(5)).boxed())
          .await;
        reality.expect("No change notification received after expectation in notifier");
        println!("Notified!");
      }
      assert_eq!(outstanding, 0, "Received a different amount than expected?");
      println!("Countdown complete");
      assert_eq!(
        &*watch_rx.borrow(),
        &0,
        "Tracked value did not return to 0 after countdown"
      );
    }

    /// Verifies that tracked items clean up their tracking registration if dropped mid-execution
    #[tokio::test]
    async fn bound_counter_tracking_lazy_in_progress_drop() {
      #[derive(Debug, PartialEq, Eq)]
      struct TestResultValue;

      use tokio::sync::watch;
      let (watch_tx, mut watch_rx) = watch::channel(Default::default());
      let (updater, counter_holder) = (Arc::new(watch_tx), Arc::new(Arc::new(())));

      const COUNT: usize = 100usize;

      assert_eq!(
        *watch_rx.borrow(),
        0,
        "Tracked value should be zero before count-up"
      );
      let barrier = Arc::new(tokio::sync::Notify::new());
      let mut pool = Vec::new();
      for _ in 0..COUNT {
        let tracker = BoundCounterTracker::new(updater.clone(), counter_holder.clone());
        let barrier = barrier.clone();
        pool.push(Box::pin(
          async move {
            barrier.notified().await;
            TestResultValue
          }
          .track(tracker),
        ));
        assert_eq!(
          *watch_rx.borrow(),
          0,
          "Tracked value should not go up in lazy-registered count-up"
        );
      }
      println!("Lazy-registered count-up construction complete");

      let count_up_notification = watch_rx.changed();
      // Poll all tasks once
      for item in pool.iter_mut() {
        let res = futures::poll!(item);
        assert_eq!(
          res,
          Poll::Pending,
          "Test futures must not complete until the notification is prepared"
        );
      }
      println!("Awaiting lazy-registered count-up side-effects...");
      let reality = count_up_notification
        .map(|_| ())
        .poll_until(tokio::time::sleep(Duration::from_secs(5)).boxed())
        .await;
      reality.expect("No change notification received after expectation in count-up notifier");
      println!("Lazy-registered count-up side-effects noted!");

      // None of the futures in the pool have actually returned their values;
      // Delete them one by one and verify that a notification occurs for each.
      let mut outstanding = COUNT;
      for fut in pool.into_iter() {
        assert!(outstanding > 0, "Received more than expected?");
        outstanding -= 1;
        let expectation = watch_rx.changed();
        drop(fut);
        print!("Waiting for a notification, or 5 seconds... ");
        let reality = expectation
          .map(|_| ())
          .poll_until(tokio::time::sleep(Duration::from_secs(5)).boxed())
          .await;
        reality.expect("No change notification received after expectation in notifier");
        println!("Notified!");
      }
      assert_eq!(outstanding, 0, "Received a different amount than expected?");
      println!("Countdown complete");

      assert_eq!(
        &*watch_rx.borrow(),
        &0,
        "Tracked value did not return to 0 after countdown"
      );
    }

    #[tokio::test]
    async fn bound_counter_tracking_lazy() {
      #[derive(Debug, PartialEq, Eq)]
      struct TestResultValue;

      use tokio::sync::watch;
      let (watch_tx, mut watch_rx) = watch::channel(Default::default());
      let (updater, counter_holder) = (Arc::new(watch_tx), Arc::new(Arc::new(())));

      const COUNT: usize = 100usize;

      assert_eq!(
        *watch_rx.borrow(),
        0,
        "Tracked value should be zero before count-up"
      );
      let barrier = Arc::new(tokio::sync::Notify::new());
      let mut pool = Vec::new();
      for _ in 0..COUNT {
        let tracker = BoundCounterTracker::new(updater.clone(), counter_holder.clone());
        let barrier = barrier.clone();
        pool.push(Box::pin(
          async move {
            barrier.notified().await;
            TestResultValue
          }
          .track(tracker),
        ));
        assert_eq!(
          *watch_rx.borrow(),
          0,
          "Tracked value should not go up in lazy-registered count-up"
        );
      }
      println!("Lazy-registered count-up construction complete");

      let count_up_notification = watch_rx.changed();
      // Poll all tasks once
      for item in pool.iter_mut() {
        let res = futures::poll!(item);
        assert_eq!(
          res,
          Poll::Pending,
          "Test futures must not complete until the notification is prepared"
        );
      }
      println!("Awaiting lazy-registered count-up side-effects...");
      let reality = count_up_notification
        .map(|_| ())
        .poll_until(tokio::time::sleep(Duration::from_secs(5)).boxed())
        .await;
      reality.expect("No change notification received after expectation in count-up notifier");
      println!("Lazy-registered count-up side-effects noted!");

      // Tell queued futures they can now complete

      barrier.notify_waiters();
      let mut outstanding = COUNT;
      for fut in pool.into_iter() {
        assert!(outstanding > 0, "Received more than expected?");
        outstanding -= 1;
        let expectation = watch_rx.changed();
        let res = fut
          .poll_until(tokio::time::sleep(Duration::from_secs(5)).boxed())
          .await
          .expect("Timed out waiting for future completion");
        assert_eq!(res, TestResultValue, "Future must produce the test result");
        print!("Waiting for a notification, or 5 seconds... ");
        let reality = expectation
          .map(|_| ())
          .poll_until(tokio::time::sleep(Duration::from_secs(5)).boxed())
          .await;
        reality.expect("No change notification received after expectation in notifier");
        println!("Notified!");
      }
      assert_eq!(outstanding, 0, "Received a different amount than expected?");
      println!("Countdown complete");

      assert_eq!(
        &*watch_rx.borrow(),
        &0,
        "Tracked value did not return to 0 after countdown"
      );
    }
  }
}

mod stream_ext_ext {
  use std::sync::Arc;

  use ::futures::{
    future::{BoxFuture, Future, FutureExt},
    stream::{StreamExt, TryForEachConcurrent, TryStream, TryStreamExt},
  };

  use crate::ext::future::FutureExtExt;

  use super::bound_counter::BoundCounterTracker;
  pub trait StreamExtExt: StreamExt + private::Sealed {
    // TODO: Replace with https://docs.rs/futures/latest/futures/stream/struct.FuturesUnordered.html#method.len
    // TODO: Notify subscribers of changes when `push` or `next` return; "pull" by polling on Next or the upstream future.
    // TODO: The above eliminates the need for needlessly-complex [BoundCounter] trackers
    /// Monitors the number of outstanding futures being run concurrently
    fn try_for_each_concurrent_monitored<'f, Fut, F>(
      self,
      limit: impl Into<Option<usize>>,
      updater: tokio::sync::watch::Sender<usize>,
      f: F,
    ) -> TryForEachConcurrent<
      Self,
      BoxFuture<'f, Result<(), Self::Error>>,
      Box<dyn (FnMut(Self::Ok) -> BoxFuture<'f, Result<(), Self::Error>>) + Send + Sync + 'f>,
    >
    where
      Self: TryStreamExt + Send,
      <Self as TryStream>::Ok: Send + 'f,
      <Self as TryStream>::Error: Send + 'f,
      F: (FnMut(Self::Ok) -> Fut) + Send + Sync + 'f,
      Fut: Future<Output = Result<(), Self::Error>> + Send + 'f,
      Self: Sized,
    {
      let mut f = f;
      let (updater, counter_holder) = (Arc::new(updater), Arc::new(Arc::new(())));
      self.try_for_each_concurrent(
        limit,
        Box::new(move |ok| {
          let tracker =
            BoundCounterTracker::new_preregistered(updater.clone(), counter_holder.clone());
          f(ok).track(tracker).boxed()
        }),
      )
    }
  }

  impl<S: ?Sized + StreamExt> StreamExtExt for S {}

  mod private {
    pub trait Sealed {}

    impl<S: ?Sized + ::futures::stream::StreamExt> Sealed for S {}
  }
}

pub use stream_ext_ext::StreamExtExt;

#[cfg(test)]
mod tests {
  use futures::{
    future::{self, BoxFuture, FutureExt},
    stream::{self, BoxStream, StreamExt},
  };
  use tokio::sync::{oneshot, watch};

  use super::StreamExtExt;

  /// Verifies that the concurrent monitoring combinator can count the number of
  /// running items by running through several "phases" wherein a differing number
  /// of concurrent tasks is expected to be present and running
  #[tokio::test]
  async fn concurrent_monitoring() {
    // Use shared-future oneshot channels as a substitute for `Notify` that
    // continues to issue notifications to all future requests once set.
    //
    // This allows us to preemptively notify items that aren't yet waiting
    let historical_notifier_channel = || {
      let (send, recv) = oneshot::channel::<()>();
      (send, recv.map(|_| ()).boxed().shared())
    };
    let (phase_one_send, phase_one) = historical_notifier_channel();
    let (phase_two_send, phase_two) = historical_notifier_channel();
    let (phase_three_send, phase_three) = historical_notifier_channel();
    let mut items: Vec<BoxFuture<'static, Result<(), ()>>> = Vec::new();
    for _ in 1u32..=5 {
      // Add items that end after phase two
      items.push({
        let phase_two = phase_two.clone();
        async move {
          phase_two.await;
          Result::<(), ()>::Ok(())
        }
        .boxed()
      });
    }
    for _ in 1u32..=5 {
      // Add items that end after phase two and three
      items.push({
        let phase_two = phase_two.clone();
        let phase_three = phase_three.clone();
        async move {
          phase_two.await;
          phase_three.await;
          Result::<(), ()>::Ok(())
        }
        .boxed()
      });
    }

    let phased_items: BoxStream<'static, Result<BoxFuture<'static, Result<(), ()>>, ()>> =
      stream::once({
        let phase_one = phase_one.clone();
        async move {
          println!("Item stream awaiting notification");
          phase_one.await;
          println!("Item stream notified, producing iterations...");
          stream::iter(items.into_iter())
        }
      })
      .flatten_unordered(None)
      .map(Result::<_, ()>::Ok)
      .boxed();

    let (sender, mut watcher) = watch::channel(0);

    const CONCURRENCY_LIMIT: usize = 8;
    let runner = async move {
      // By limiting to 8 at a time, we ensure that the system never loads more than 8
      phased_items
        // Note that adding a `+ 1` after the `CONCURRENCY_LIMIT` below crashes
        // with a concurrency-limit-exceeded panic, as of writing.
        //
        // If this invariant can be verified by the test itself without
        // massive code duplication, that'd be a nice assertion to have.
        .try_for_each_concurrent_monitored(Some(CONCURRENCY_LIMIT), sender, |f| async {
          println!("Starting an item");
          let res = f.await;
          println!("Finished an item");
          res
        })
        .await
        .unwrap();
    };
    let monitor = async move {
      println!("Checking pre-phase count");
      assert_eq!(0, *watcher.borrow(), "Monitor must start at 0");

      println!("Notifying phase one");
      phase_one_send.send(()).unwrap();
      for i in 1..=10 {
        println!("Waiting for watcher to change (iter {})", i);
        watcher.changed().await.unwrap();
        if *watcher.borrow() == 8 {
          println!("Phase one reached monitor value of 8 at iteration {}", i);
          break;
        } else {
          assert!(
            *watcher.borrow() <= CONCURRENCY_LIMIT,
            "Watcher exceeded concurrency limit!"
          );
          println!(
            "Watcher sees a set of {} items in iteration {}",
            *watcher.borrow(),
            i
          );
        }
      }
      for _ in 1..=100 {
        // Allow some poll events to pass for the watcher to exceed the CONCURRENCY_LIMIT if it would
        tokio::task::yield_now().await;
        assert_eq!(
          CONCURRENCY_LIMIT,
          *watcher.borrow(),
          "Monitor must see {} items in phase 1",
          CONCURRENCY_LIMIT,
        );
      }

      println!("Notifying phase two");
      phase_two_send.send(()).unwrap();
      for i in 1..=5 {
        println!("Waiting for watcher to change (iter {})", i);
        watcher.changed().await.unwrap();
        if *watcher.borrow() == 5 {
          println!("Phase one reached monitor value of 5 at iteration {}", i);
          break;
        } else {
          println!(
            "Watcher sees a set of {} items in iteration {}",
            *watcher.borrow(),
            i
          );
        }
      }
      assert_eq!(5, *watcher.borrow(), "Monitor must see 5 items in phase 2");

      println!("Notifying phase three");
      phase_three_send.send(()).unwrap();
      for i in 1..=5 {
        println!("Waiting for watcher to change (iter {})", i);
        watcher.changed().await.unwrap();
        if *watcher.borrow() == 0 {
          println!("Phase one reached monitor value of 0 at iteration {}", i);
          break;
        } else {
          println!(
            "Watcher sees a set of {} items in iteration {}",
            *watcher.borrow(),
            i
          );
        }
      }
      assert_eq!(
        0,
        *watcher.borrow(),
        "Monitor must end phase 3 at 0 active items"
      );
    };
    future::join(runner, monitor).await;
  }
}
