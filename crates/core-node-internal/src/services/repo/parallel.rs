//! Runs independent pieces of work at once on a bounded number of threads,
//! and gives their results back in the order of the work. A refresh reads
//! its repositories this way: the reads wait on the network side by side,
//! and the refresh still merges what they publish in repository id order.

use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::sync::mpsc;

/// Runs `work` on every item of `items`, on at most `max_threads` threads at
/// once, and returns what each call returned, in the order of `items`.
///
/// Each thread takes the next item that no thread has taken, until none is
/// left, so a slow item holds up one thread only. `work` reports through the
/// callback it gets: each message reaches `on_message` on the calling thread
/// while the work runs, so `on_message` needs neither `Send` nor a lock, and
/// the messages of one item arrive in the order its call sent them.
///
/// A panic in `work` resumes on the calling thread when every thread has
/// stopped.
pub(crate) fn map_in_parallel<T, R, M>(
    items: Vec<T>,
    max_threads: NonZeroUsize,
    on_message: &mut dyn FnMut(M),
    work: impl Fn(T, &mut dyn FnMut(M)) -> R + Sync,
) -> Vec<R>
where
    T: Send,
    R: Send,
    M: Send,
{
    let thread_count = max_threads.get().min(items.len());
    let queue = Mutex::new(items.into_iter().enumerate());
    let (sender, receiver) = mpsc::channel::<M>();

    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..thread_count)
            .map(|_| {
                let sender = sender.clone();
                let (queue, work) = (&queue, &work);
                scope.spawn(move || {
                    // A send fails only when the calling thread no longer
                    // listens, which it does only while it unwinds.
                    let mut send = |message: M| {
                        let _ = sender.send(message);
                    };
                    let mut done = Vec::new();
                    loop {
                        // Its own statement, so the lock is released before
                        // the work starts.
                        let next = queue.lock().next();
                        let Some((index, item)) = next else {
                            return done;
                        };
                        done.push((index, work(item, &mut send)));
                    }
                })
            })
            .collect();
        // The threads now hold the only senders, so the loop ends when every
        // thread has stopped.
        drop(sender);
        for message in receiver {
            on_message(message);
        }

        let mut done: Vec<(usize, R)> = threads
            .into_iter()
            .flat_map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect();
        done.sort_unstable_by_key(|(index, _)| *index);
        done.into_iter().map(|(_, result)| result).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn threads(count: usize) -> NonZeroUsize {
        NonZeroUsize::new(count).expect("a test runs at least one thread")
    }

    /// One item of a chain in which each item finishes only after the item
    /// that follows it.
    struct ChainedItem {
        index: usize,
        /// Tells this item that the next item has finished; `None` for the
        /// last item.
        next_finished: Option<mpsc::Receiver<()>>,
        /// Tells the item before that this one has finished; `None` for the
        /// first item.
        tell_previous: Option<mpsc::Sender<()>>,
    }

    fn chain(count: usize) -> Vec<ChainedItem> {
        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..count).map(|_| mpsc::channel::<()>()).unzip();
        receivers
            .into_iter()
            .enumerate()
            .map(|(index, next_finished)| ChainedItem {
                index,
                next_finished: (index + 1 < count).then_some(next_finished),
                tell_previous: index
                    .checked_sub(1)
                    .map(|previous| senders[previous].clone()),
            })
            .collect()
    }

    /// The items finish in the reverse of their order. This also proves that
    /// they run at once: one at a time, the first item would wait for ever.
    #[test]
    fn returns_the_results_in_the_order_of_the_items_whatever_order_they_finish_in() {
        const COUNT: usize = 4;
        let finished = Mutex::new(Vec::new());

        let results = map_in_parallel(chain(COUNT), threads(COUNT), &mut |_: ()| {}, |item, _| {
            if let Some(next_finished) = item.next_finished {
                next_finished.recv().expect("the next item finishes");
            }
            finished.lock().push(item.index);
            if let Some(tell_previous) = item.tell_previous {
                tell_previous.send(()).expect("the item before waits");
            }
            item.index * 10
        });

        assert_eq!(*finished.lock(), vec![3, 2, 1, 0]);
        assert_eq!(results, vec![0, 10, 20, 30]);
    }

    /// Every item waits until two others run beside it, so the call returns
    /// only if three items run at once, each time.
    #[test]
    fn runs_the_items_on_max_threads_threads_at_once_and_never_more() {
        const MAX_THREADS: usize = 3;
        let together = Barrier::new(MAX_THREADS);
        let running = AtomicUsize::new(0);
        let most_running = AtomicUsize::new(0);
        let thread_ids = Mutex::new(HashSet::new());

        let results = map_in_parallel(
            (0..3 * MAX_THREADS).collect(),
            threads(MAX_THREADS),
            &mut |_: ()| {},
            |item, _| {
                let now_running = running.fetch_add(1, Ordering::SeqCst) + 1;
                most_running.fetch_max(now_running, Ordering::SeqCst);
                thread_ids.lock().insert(std::thread::current().id());
                together.wait();
                running.fetch_sub(1, Ordering::SeqCst);
                item
            },
        );

        assert_eq!(results, (0..3 * MAX_THREADS).collect::<Vec<_>>());
        assert_eq!(most_running.load(Ordering::SeqCst), MAX_THREADS);
        assert_eq!(thread_ids.lock().len(), MAX_THREADS);
    }

    #[test]
    fn forwards_every_message_in_the_order_its_item_sent_it() {
        let mut received = Vec::new();

        map_in_parallel(
            vec!["a", "b", "c"],
            threads(3),
            &mut |message: String| received.push(message),
            |item, send| {
                send(format!("{item} started"));
                send(format!("{item} finished"));
            },
        );

        assert_eq!(received.len(), 6, "{received:?}");
        for item in ["a", "b", "c"] {
            let position = |message: String| {
                received
                    .iter()
                    .position(|received| *received == message)
                    .unwrap_or_else(|| panic!("`{message}` arrives: {received:?}"))
            };
            assert!(
                position(format!("{item} started")) < position(format!("{item} finished")),
                "{received:?}"
            );
        }
    }

    #[test]
    fn resumes_a_panic_of_the_work_on_the_calling_thread() {
        let panic = std::panic::catch_unwind(|| {
            map_in_parallel(vec![1, 2, 3], threads(2), &mut |_: ()| {}, |item, _| {
                assert_ne!(item, 2, "item 2 cannot be read");
                item
            })
        })
        .expect_err("the panic of item 2 reaches the caller");

        let message = panic
            .downcast_ref::<String>()
            .expect("an assertion panics with a formatted message");
        assert!(message.contains("item 2 cannot be read"), "{message}");
    }

    #[test]
    fn returns_nothing_for_no_items() {
        let mut messages = 0;

        let results: Vec<()> = map_in_parallel(
            Vec::<()>::new(),
            threads(4),
            &mut |_: ()| messages += 1,
            |item, _| item,
        );

        assert!(results.is_empty());
        assert_eq!(messages, 0);
    }
}
