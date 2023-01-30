// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

mod bound_counter {
  use crate::util::dropkick::{Dropkick, DropkickSync};
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
}

mod stream_ext_ext {
  use std::sync::Arc;

  use ::futures::{
    future::{BoxFuture, Future, FutureExt},
    stream::{StreamExt, TryForEachConcurrent, TryStream, TryStreamExt},
  };
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
          let bound_counter = super::bound_counter::BoundCounter::new(&updater, &counter_holder);
          let fut = f(ok);
          async move {
            let bound_counter = bound_counter;
            let res = fut.await;
            drop(bound_counter);
            res
          }
          .boxed()
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
